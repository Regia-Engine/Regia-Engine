// SPDX-License-Identifier: MPL-2.0
//! The `locomotion` debug overlay (ADR-8729): what the gait stepper, the foot planter and the leg
//! IK decided this frame, drawn with gizmos so a walker or a foot planter can be tuned by eye.
//!
//! One gizmo config group, [`LocomotionGizmos`] (name `locomotion`), off by default, with three
//! sub-toggles (all on by default):
//!
//! - `gait` (every entity with a [`GaitRig`] and its [`GaitState`]): the hips (neutral
//!   footholds, with a line from the hip to the foot), the foot targets (planted and swinging
//!   colors), each swing arc sampled from the stepper's own arc formula
//!   ([`swing_arc_point`]) from lift-off to landing, the support polygon of the planted feet, the
//!   static stability margin as a ring around the body, refused markers (a hanging foot, and the
//!   body when [`GaitState::refused`] grew this frame), forced-step markers, and the body
//!   attitude frame (the body joint's world rotation, else the heading).
//! - `foot_placement` (every [`FootPlacement`] rig): the probe rays and their hits with the
//!   ground normal (the overlay casts the same ray the planter cast: from the pin, else the
//!   ankle, `probe_height` up, `probe_length` down), the pins with a bar as tall as their lock
//!   weight, the tilted sole, the pelvis offset, and a ring as wide as each foot's slide since
//!   touchdown.
//! - `ik` (the legs of both): the chain joints and bones, the IK target, the knee pole (foot
//!   placement legs), the reach sphere around the chain root, and the joint limits of
//!   FABRIK/CCD chains (a cone per ball joint, an arc per hinge).
//!
//! Turn it on with `debug.set_gizmos { "group": "locomotion", "enabled": true }` and pick
//! sub-toggles with `"toggles": { "ik": false }`; `debug.gizmos` lists the toggles. Colors come
//! from theme tokens ([`LocomotionGizmoColors`]). [`LocomotionGizmoStats`] counts what the last
//! drawn frame drew.
//!
//! The draw system runs only while the group is enabled, so the overlay costs nothing when it
//! is off. On, a frame stops starting new draws once it holds [`MAX_LOCOMOTION_SEGMENTS`]
//! segments (the paths overlay's budget): the rigs and chains past it are counted in
//! [`LocomotionGizmoStats::skipped`] instead of drawn, so a crowd of walkers cannot grow the
//! frame without bound. It reads presentation state and never writes it; nothing here feeds a simulation
//! checksum. Rigs are drawn with their `Transform` as the world placement, as the animation
//! systems use it (a root-level rig). Hosts that step a walker by hand call [`draw_gait`]
//! themselves.

use std::f32::consts::TAU;

use regia_animation::chain_ik::JointLimit;
use regia_animation::foot_ik::{FootPlacement, FootPlantReport, GroundProbes};
use regia_animation::gait::{FootMode, GaitMode, GaitParams, GaitState, MAX_LEGS, swing_arc_point};
use regia_animation::gait_rig::{GaitLegChain, GaitRig};
use regia_animation::{JointGlobals, Skeleton, SkinnedMesh};
use regia_app::prelude::*;
use regia_core::math::detmath::{det_quat_from_axis_angle, det_sin_cos};
use regia_core::math::{Color, Mat4, Quat, Vec3};
use regia_render::gizmos::{GizmoBuffer, GizmoConfig, GizmoConfigGroup, Gizmos, gizmo_group_enabled};
use regia_scene::Transform;

/// The `locomotion` gizmo group and its sub-toggles (the group's value in the
/// `GizmoConfigStore`). Disabled by default; every sub-toggle defaults to on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocomotionGizmos {
    /// Hips, foot targets, swing arcs, support polygon, stability margin, refused and forced
    /// markers, body attitude frame.
    pub gait: bool,
    /// Probe rays and hits, normals, pins and lock weight, tilted soles, pelvis offset, slide.
    pub foot_placement: bool,
    /// Chain joints and bones, targets, poles, reach spheres, joint limits.
    pub ik: bool,
}

impl Default for LocomotionGizmos {
    fn default() -> Self {
        Self {
            gait: true,
            foot_placement: true,
            ik: true,
        }
    }
}

impl GizmoConfigGroup for LocomotionGizmos {
    const NAME: &'static str = "locomotion";

    fn default_config() -> GizmoConfig {
        GizmoConfig {
            enabled: false,
            depth_bias: -0.002,
            ..GizmoConfig::default()
        }
    }
}

impl LocomotionGizmos {
    /// The sub-toggle names, in `debug.gizmos` order.
    pub const TOGGLES: [&'static str; 3] = ["gait", "foot_placement", "ik"];

    /// The sub-toggle named `name`, or `None` for an unknown name.
    #[must_use]
    pub fn toggle(&self, name: &str) -> Option<bool> {
        match name {
            "gait" => Some(self.gait),
            "foot_placement" => Some(self.foot_placement),
            "ik" => Some(self.ik),
            _ => None,
        }
    }

    /// Sets the sub-toggle named `name`. Returns `false` (and changes nothing) for an unknown
    /// name.
    pub fn set_toggle(&mut self, name: &str, on: bool) -> bool {
        let slot = match name {
            "gait" => &mut self.gait,
            "foot_placement" => &mut self.foot_placement,
            "ik" => &mut self.ik,
            _ => return false,
        };
        *slot = on;
        true
    }
}

/// Most segments the overlay starts drawing in one frame (the paths overlay's budget). The
/// check runs before each walker's gait, each IK chain and each foot planter, so a frame passes
/// it by at most that one draw. Counted conservatively: line-list segments plus every
/// line-strip entry.
pub const MAX_LOCOMOTION_SEGMENTS: usize = 20_000;

/// Segments of a foot, pin, sole, slide or stability ring.
pub const RING_SEGMENTS: usize = 12;
/// Segments of a sampled swing arc.
pub const ARC_SEGMENTS: usize = 16;
/// Segments of each of the three great circles of a reach sphere.
pub const REACH_SEGMENTS: usize = 16;
/// Segments of a hinge limit's arc.
pub const HINGE_SEGMENTS: usize = 12;
/// Rays from the apex of a cone limit to its rim.
pub const CONE_RAYS: usize = 4;

/// Radius of a foot target ring, meters.
pub const FOOT_RADIUS: f32 = 0.07;
/// Radius of a pin ring, meters.
pub const PIN_RADIUS: f32 = 0.05;
/// Radius of a sole ring, meters.
pub const SOLE_RADIUS: f32 = 0.1;
/// Half size of a marker cross, meters.
pub const MARKER_SIZE: f32 = 0.04;
/// Length of a ground normal line, meters.
pub const NORMAL_LENGTH: f32 = 0.25;
/// Height of a pin's lock bar at lock weight 1, meters.
pub const LOCK_BAR_LENGTH: f32 = 0.3;
/// Length of each axis of the body attitude frame, meters.
pub const ATTITUDE_LENGTH: f32 = 0.3;
/// Slides at or below this, meters, draw no ring.
pub const SLIDE_EPSILON: f32 = 1.0e-4;

/// Overlay colors, one per role, each read from a theme token ([`Self::TOKENS`]). The default is
/// the built-in default theme's tokens; a host that switches themes builds one with
/// [`Self::from_tokens`] (or [`Self::from_theme`]) and replaces the resource.
#[derive(Resource, Clone, Copy, Debug, PartialEq)]
pub struct LocomotionGizmoColors {
    /// Planted foot targets (`color.success`).
    pub planted: Color,
    /// Swinging foot targets and swing arcs (`color.warning`).
    pub swinging: Color,
    /// Refused markers: a hanging foot, a refused landing (`color.danger`).
    pub refused: Color,
    /// Forced-step markers (`color.info`).
    pub forced: Color,
    /// Hips and hip-to-foot lines (`color.text-muted`).
    pub hip: Color,
    /// The support polygon (`color.accent`).
    pub support: Color,
    /// The stability ring while statically stable (`color.success`).
    pub stable: Color,
    /// The stability ring while statically unstable (`color.danger`).
    pub unstable: Color,
    /// Attitude frame and limit axis: right, +X (`color.danger`).
    pub axis_x: Color,
    /// Attitude frame: up, +Y (`color.success`).
    pub axis_y: Color,
    /// Attitude frame: forward (`color.accent`).
    pub axis_z: Color,
    /// Probe rays (`color.text-muted`).
    pub probe: Color,
    /// Probe hits (`color.focus-ring`).
    pub probe_hit: Color,
    /// Ground normals (`color.accent`).
    pub normal: Color,
    /// Pins and their lock bars (`color.info`).
    pub pin: Color,
    /// Tilted soles (`color.success`).
    pub sole: Color,
    /// The pelvis offset (`color.warning`).
    pub pelvis: Color,
    /// Slide rings (`color.danger`).
    pub slide: Color,
    /// IK bones (`color.text`).
    pub bone: Color,
    /// IK joints (`color.text-muted`).
    pub joint: Color,
    /// IK targets (`color.warning`).
    pub target: Color,
    /// Knee poles (`color.info`).
    pub pole: Color,
    /// Reach spheres (`color.border-strong`).
    pub reach: Color,
    /// Joint limit cones and hinge arcs (`color.accent-hover`).
    pub limit: Color,
}

impl LocomotionGizmoColors {
    /// Every role and the theme token it reads, in field order.
    pub const TOKENS: [(&'static str, &'static str); 24] = [
        ("planted", "color.success"),
        ("swinging", "color.warning"),
        ("refused", "color.danger"),
        ("forced", "color.info"),
        ("hip", "color.text-muted"),
        ("support", "color.accent"),
        ("stable", "color.success"),
        ("unstable", "color.danger"),
        ("axis_x", "color.danger"),
        ("axis_y", "color.success"),
        ("axis_z", "color.accent"),
        ("probe", "color.text-muted"),
        ("probe_hit", "color.focus-ring"),
        ("normal", "color.accent"),
        ("pin", "color.info"),
        ("sole", "color.success"),
        ("pelvis", "color.warning"),
        ("slide", "color.danger"),
        ("bone", "color.text"),
        ("joint", "color.text-muted"),
        ("target", "color.warning"),
        ("pole", "color.info"),
        ("reach", "color.border-strong"),
        ("limit", "color.accent-hover"),
    ];

    /// The colors from `token`, a lookup of theme color tokens by key. A token the lookup does
    /// not know reads `color.text`, then white.
    #[must_use]
    pub fn from_tokens(token: impl Fn(&str) -> Option<Color>) -> Self {
        let fallback = token("color.text").unwrap_or(Color::WHITE);
        let c = |i: usize| token(Self::TOKENS[i].1).unwrap_or(fallback);
        Self {
            planted: c(0),
            swinging: c(1),
            refused: c(2),
            forced: c(3),
            hip: c(4),
            support: c(5),
            stable: c(6),
            unstable: c(7),
            axis_x: c(8),
            axis_y: c(9),
            axis_z: c(10),
            probe: c(11),
            probe_hit: c(12),
            normal: c(13),
            pin: c(14),
            sole: c(15),
            pelvis: c(16),
            slide: c(17),
            bone: c(18),
            joint: c(19),
            target: c(20),
            pole: c(21),
            reach: c(22),
            limit: c(23),
        }
    }

    /// The colors of a resolved theme.
    #[must_use]
    pub fn from_theme(theme: &regia_ui::theme_file::ResolvedTheme) -> Self {
        Self::from_tokens(|key| theme.color(key).map(|c| Color::from(c.0)))
    }
}

impl Default for LocomotionGizmoColors {
    /// The built-in default theme's tokens (`regia-dark`).
    fn default() -> Self {
        let set = regia_ui::theme_resolve::builtin_theme_set();
        match set.resolve(regia_ui::theme_resolve::DEFAULT_THEME, None) {
            Ok(theme) => Self::from_theme(&theme),
            Err(error) => {
                tracing::warn!(target: "regia_framework", %error, "locomotion gizmos: the default theme did not resolve");
                Self::from_tokens(|_| None)
            }
        }
    }
}

/// What the last drawn frame of the overlay drew. Counts are reset at the start of each drawn
/// frame; a frame with the group off does not run the draw and leaves them (and
/// [`Self::frames`]) unchanged.
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LocomotionGizmoStats {
    /// Frames the draw system ran (the group was enabled).
    pub frames: u64,
    /// Walkers drawn (gait).
    pub walkers: u32,
    /// Planted foot target rings.
    pub planted_feet: u32,
    /// Swinging foot target rings.
    pub swinging_feet: u32,
    /// Sampled swing arcs.
    pub swing_arcs: u32,
    /// Hip crosses (each with its hip-to-foot line).
    pub hips: u32,
    /// Edges of the support polygons.
    pub support_edges: u32,
    /// Stability margin rings.
    pub margin_rings: u32,
    /// Refused markers.
    pub refused_markers: u32,
    /// Forced-step markers.
    pub forced_markers: u32,
    /// Body attitude frames.
    pub attitude_frames: u32,
    /// Foot placement rigs drawn.
    pub planters: u32,
    /// Probe rays.
    pub probes: u32,
    /// Probe hits (each with its normal).
    pub probe_hits: u32,
    /// Pins (each with its lock bar).
    pub pins: u32,
    /// Tilted sole rings.
    pub soles: u32,
    /// Slide rings.
    pub slides: u32,
    /// Pelvis offset markers.
    pub pelvis_markers: u32,
    /// IK chains drawn.
    pub chains: u32,
    /// IK joint crosses.
    pub joints: u32,
    /// IK bones.
    pub bones: u32,
    /// IK target crosses (each with its error line).
    pub targets: u32,
    /// Knee poles (each a line and a cross).
    pub poles: u32,
    /// Reach spheres (three great circles each).
    pub reach_spheres: u32,
    /// Cone limits.
    pub cone_limits: u32,
    /// Hinge limits.
    pub hinge_limits: u32,
    /// Line-list segments the frame drew.
    pub lines: u32,
    /// Line strips the frame drew.
    pub strips: u32,
    /// Line-strip vertices the frame drew (separators excluded).
    pub strip_vertices: u32,
    /// Draws not started because the frame already held [`MAX_LOCOMOTION_SEGMENTS`] segments:
    /// one per walker gait, IK chain or foot planter skipped.
    pub skipped: u32,
}

impl LocomotionGizmoStats {
    fn reset_frame(&mut self) {
        *self = Self {
            frames: self.frames.wrapping_add(1),
            ..Self::default()
        };
    }
}

/// Registers the group, its colors and stats, and the draw system (`PostUpdate`, after
/// transforms; only while the group is enabled). Added by
/// [`crate::debug_gizmos::DebugGizmosPlugin`].
#[derive(Default)]
pub struct LocomotionGizmosPlugin;

impl Plugin for LocomotionGizmosPlugin {
    fn build(&self, app: &mut App) {
        use regia_render::gizmos::AppGizmoBuilder;
        if !app.world().contains_resource::<LocomotionGizmoColors>() {
            app.init_resource::<LocomotionGizmoColors>();
        }
        app.init_resource::<LocomotionGizmoStats>()
            .init_gizmo_group::<LocomotionGizmos>()
            .add_systems(
                PostUpdate,
                draw_locomotion
                    .run_if(gizmo_group_enabled::<LocomotionGizmos>)
                    .after(PostUpdateSet::TransformPropagate),
            );
    }
}

/// An upper bound of the segments in `buffer`: line-list pairs plus every line-strip entry (a
/// strip of `k` vertices draws `k - 1` segments and ends with one separator). O(1).
fn segment_bound<G: GizmoConfigGroup>(buffer: &GizmoBuffer<G>) -> usize {
    buffer.list_positions.len() / 2 + buffer.strip_positions.len()
}

/// `true` (and one more [`LocomotionGizmoStats::skipped`]) when the frame is at the budget.
fn over_budget<G: GizmoConfigGroup>(buffer: &GizmoBuffer<G>, stats: &mut LocomotionGizmoStats) -> bool {
    let over = segment_bound(buffer) >= MAX_LOCOMOTION_SEGMENTS;
    if over {
        stats.skipped = stats.skipped.saturating_add(1);
    }
    over
}

/// Each walker's [`GaitState::refused`] at the last drawn frame, so the overlay marks the body
/// on a frame whose count grew. Entries of walkers not seen on a frame are dropped, so the list
/// holds at most the walkers alive on the latest drawn frame: a spawn and despawn loop reuses
/// its capacity instead of growing it.
///
/// Public only because it is the draw system's `Local` state; it has no public API.
#[derive(Debug, Default)]
pub struct RefusedSeen {
    /// Walker, its count, and the drawn frame it was last seen on.
    entries: Vec<(Entity, u32, u64)>,
}

impl RefusedSeen {
    /// Record `entity`'s count on `frame`; `true` when it grew since the walker's last frame.
    fn observe(&mut self, entity: Entity, refused: u32, frame: u64) -> bool {
        match self.entries.iter_mut().find(|(e, ..)| *e == entity) {
            Some((_, seen, at)) => {
                let grew = refused > *seen;
                *seen = refused;
                *at = frame;
                grew
            }
            None => {
                self.entries.push((entity, refused, frame));
                false
            }
        }
    }

    /// Drop the walkers not seen on `frame`. Keeps the capacity.
    fn retire(&mut self, frame: u64) {
        self.entries.retain(|&(_, _, at)| at == frame);
    }
}

/// The world placement of a rig-local point (the animation systems' convention).
fn to_world(t: &Transform, local: Vec3) -> Vec3 {
    t.translation + t.rotation * (local * t.scale)
}

fn rotation_of(m: &Mat4) -> Quat {
    m.to_scale_rotation_translation().1.normalize()
}

/// World position of joint `joint`, or `None` past the globals.
fn joint_world(t: &Transform, globals: &[Mat4], joint: u32) -> Option<Vec3> {
    let m = globals.get(joint as usize)?;
    Some(to_world(t, m.w_axis.truncate()))
}

/// A closed circle of `segments` around `center` in the plane with `normal`: one strip of
/// `segments + 1` vertices.
fn ring<G: GizmoConfigGroup>(
    buffer: &mut GizmoBuffer<G>,
    center: Vec3,
    normal: Vec3,
    radius: f32,
    segments: usize,
    color: Color,
) {
    let (u, v) = normal.normalize_or(Vec3::Y).any_orthonormal_pair();
    let step = TAU / segments as f32;
    buffer.linestrip(
        (0..=segments).map(|i| {
            let (s, c) = det_sin_cos(step * (i % segments) as f32);
            center + (u * c + v * s) * radius
        }),
        color,
    );
}

/// Three axis lines through `p`.
fn cross<G: GizmoConfigGroup>(buffer: &mut GizmoBuffer<G>, p: Vec3, size: f32, color: Color) {
    for axis in [Vec3::X, Vec3::Y, Vec3::Z] {
        buffer.line(p - axis * size, p + axis * size, color);
    }
}

/// Two diagonal lines on XZ through `p`.
fn x_mark<G: GizmoConfigGroup>(buffer: &mut GizmoBuffer<G>, p: Vec3, size: f32, color: Color) {
    buffer.line(
        p + Vec3::new(-size, 0.0, -size),
        p + Vec3::new(size, 0.0, size),
        color,
    );
    buffer.line(
        p + Vec3::new(-size, 0.0, size),
        p + Vec3::new(size, 0.0, -size),
        color,
    );
}

/// A closed diamond on XZ around `p`: one strip of five vertices.
fn diamond<G: GizmoConfigGroup>(buffer: &mut GizmoBuffer<G>, p: Vec3, size: f32, color: Color) {
    buffer.linestrip(
        [
            p + Vec3::X * size,
            p + Vec3::Z * size,
            p - Vec3::X * size,
            p - Vec3::Z * size,
            p + Vec3::X * size,
        ],
        color,
    );
}

fn v3(a: [f32; 3]) -> Vec3 {
    Vec3::from_array(a)
}

/// The planted feet's convex hull on XZ, counter-clockwise, as foot indices (Andrew's monotone
/// chain, collinear points dropped). Returns the hull length; no allocation.
fn support_hull(state: &GaitState, hull: &mut [usize; 2 * MAX_LEGS]) -> usize {
    let mut planted = [0_usize; MAX_LEGS];
    let mut n = 0;
    for (i, foot) in state.feet().iter().enumerate() {
        if foot.planted && foot.position.iter().all(|v| v.is_finite()) {
            planted[n] = i;
            n += 1;
        }
    }
    let xz = |i: usize| [state.feet[i].position[0], state.feet[i].position[2]];
    let before = |a: usize, b: usize| {
        let (pa, pb) = (xz(a), xz(b));
        pa[0] < pb[0] || (pa[0] == pb[0] && pa[1] < pb[1])
    };
    for k in 1..n {
        let mut m = k;
        while m > 0 && before(planted[m], planted[m - 1]) {
            planted.swap(m - 1, m);
            m -= 1;
        }
    }
    let turn = |o: usize, a: usize, b: usize| {
        let (po, pa, pb) = (xz(o), xz(a), xz(b));
        (pa[0] - po[0]) * (pb[1] - po[1]) - (pa[1] - po[1]) * (pb[0] - po[0])
    };
    let mut h = 0;
    for &p in &planted[..n] {
        while h >= 2 && turn(hull[h - 2], hull[h - 1], p) <= 0.0 {
            h -= 1;
        }
        hull[h] = p;
        h += 1;
    }
    let lower = h + 1;
    for &p in planted[..n.saturating_sub(1)].iter().rev() {
        while h >= lower && turn(hull[h - 2], hull[h - 1], p) <= 0.0 {
            h -= 1;
        }
        hull[h] = p;
        h += 1;
    }
    if h > 1 {
        h -= 1;
    }
    // Two distinct planted feet: the chain ends on the start again; keep the segment.
    if h == 2 && xz(hull[0]) == xz(hull[1]) {
        h = 1;
    }
    h
}

/// Draws one walker's gait: hips, foot targets, swing arcs, support polygon, stability margin,
/// refused and forced markers, and the attitude frame. `attitude` is the body's world rotation
/// (its forward is −Z); `None` draws the heading frame of the state's yaw. `refused_now` marks
/// the body (a landing was refused this frame).
pub fn draw_gait<G: GizmoConfigGroup>(
    buffer: &mut GizmoBuffer<G>,
    params: &GaitParams,
    state: &GaitState,
    attitude: Option<Quat>,
    refused_now: bool,
    colors: &LocomotionGizmoColors,
    stats: &mut LocomotionGizmoStats,
) {
    if over_budget(buffer, stats) {
        return;
    }
    stats.walkers += 1;
    let legs = state.leg_count();
    let body = v3(state.position);
    let floor = body - Vec3::Y * params.clearance;
    for (i, foot) in state.feet().iter().enumerate() {
        let at = v3(foot.position);
        if !at.is_finite() {
            continue;
        }
        if foot.planted {
            stats.planted_feet += 1;
        } else {
            stats.swinging_feet += 1;
        }
        let color = if foot.planted {
            colors.planted
        } else {
            colors.swinging
        };
        ring(buffer, at, v3(foot.normal), FOOT_RADIUS, RING_SEGMENTS, color);
        if let Some(neutral) = state.neutral_foothold(params, i).map(v3) {
            stats.hips += 1;
            cross(buffer, neutral, MARKER_SIZE, colors.hip);
            buffer.line(Vec3::new(neutral.x, body.y, neutral.z), at, colors.hip);
        }
        match state.foot_modes[i] {
            FootMode::Hanging => {
                stats.refused_markers += 1;
                x_mark(buffer, at, MARKER_SIZE * 2.0, colors.refused);
            }
            FootMode::Forced | FootMode::Waiting => {
                stats.forced_markers += 1;
                diamond(buffer, at, MARKER_SIZE * 2.0, colors.forced);
            }
            FootMode::Tripod => {}
        }
        if let (Some(swing), Some(landing)) = (state.swing(i), state.swing_landing(params, i)) {
            stats.swing_arcs += 1;
            buffer.linestrip(
                (0..=ARC_SEGMENTS).map(|k| {
                    let u = k as f32 / ARC_SEGMENTS as f32;
                    v3(swing_arc_point(params, swing.lift_off, landing, u))
                }),
                colors.swinging,
            );
        }
    }
    let mut hull = [0_usize; 2 * MAX_LEGS];
    let h = support_hull(state, &mut hull);
    match h {
        0 | 1 => {}
        2 => {
            stats.support_edges += 1;
            buffer.line(
                v3(state.feet[hull[0]].position),
                v3(state.feet[hull[1]].position),
                colors.support,
            );
        }
        _ => {
            stats.support_edges += h as u32;
            buffer.linestrip(
                (0..=h).map(|k| v3(state.feet[hull[k % h]].position)),
                colors.support,
            );
        }
    }
    let margin = state.stability_margin;
    if margin.is_finite() && legs > 0 {
        stats.margin_rings += 1;
        let color = if margin > 0.0 {
            colors.stable
        } else {
            colors.unstable
        };
        ring(buffer, floor, Vec3::Y, margin.abs(), RING_SEGMENTS, color);
    }
    if refused_now {
        stats.refused_markers += 1;
        x_mark(buffer, floor, MARKER_SIZE * 3.0, colors.refused);
    }
    let (right, up, forward) = match attitude {
        Some(q) => (q * Vec3::X, q * Vec3::Y, q * Vec3::NEG_Z),
        None => {
            let (sin, cos) = det_sin_cos(state.yaw);
            let forward = match state.mode {
                GaitMode::Integrating => Vec3::new(sin, 0.0, cos),
                GaitMode::Driven => Vec3::new(-sin, 0.0, -cos),
            };
            (Vec3::new(cos, 0.0, -sin), Vec3::Y, forward)
        }
    };
    stats.attitude_frames += 1;
    buffer.line(body, body + right * ATTITUDE_LENGTH, colors.axis_x);
    buffer.line(body, body + up * ATTITUDE_LENGTH, colors.axis_y);
    buffer.line(body, body + forward * ATTITUDE_LENGTH, colors.axis_z);
}

/// One IK chain to draw: joints root first, in a rig.
pub struct IkChainDraw<'a> {
    /// The rig's skeleton.
    pub skeleton: &'a Skeleton,
    /// The rig's joint globals (object space).
    pub globals: &'a [Mat4],
    /// The rig's world placement.
    pub transform: &'a Transform,
    /// Joint ids, root first.
    pub joints: &'a [u32],
    /// One limit per joint (missing entries are free).
    pub limits: &'a [JointLimit],
    /// The IK target, world meters.
    pub target: Option<Vec3>,
    /// The knee pole, world meters.
    pub pole: Option<Vec3>,
    /// The chain's reach, world meters (`None`: the bind bone lengths times the rig scale).
    pub reach: Option<f32>,
}

/// Draws one IK chain: bones, joints, target with its error line, pole, reach sphere and the
/// joint limits.
pub fn draw_ik_chain<G: GizmoConfigGroup>(
    buffer: &mut GizmoBuffer<G>,
    chain: &IkChainDraw<'_>,
    colors: &LocomotionGizmoColors,
    stats: &mut LocomotionGizmoStats,
) {
    let t = chain.transform;
    let world = |j: u32| joint_world(t, chain.globals, j);
    if chain.joints.len() < 2 || chain.joints.iter().any(|&j| world(j).is_none()) {
        return;
    }
    if over_budget(buffer, stats) {
        return;
    }
    stats.chains += 1;
    stats.bones += (chain.joints.len() - 1) as u32;
    buffer.linestrip(chain.joints.iter().filter_map(|&j| world(j)), colors.bone);
    for &j in chain.joints {
        if let Some(p) = world(j) {
            stats.joints += 1;
            cross(buffer, p, MARKER_SIZE * 0.5, colors.joint);
        }
    }
    let root = chain.joints[0];
    let tip = chain.joints[chain.joints.len() - 1];
    let (Some(root_at), Some(tip_at)) = (world(root), world(tip)) else {
        return;
    };
    if let Some(target) = chain.target.filter(|p| p.is_finite()) {
        stats.targets += 1;
        cross(buffer, target, MARKER_SIZE, colors.target);
        buffer.line(tip_at, target, colors.target);
    }
    if let Some(pole) = chain.pole.filter(|p| p.is_finite())
        && let Some(mid) = chain.joints.get(1).and_then(|&j| world(j))
    {
        stats.poles += 1;
        buffer.line(mid, pole, colors.pole);
        cross(buffer, pole, MARKER_SIZE, colors.pole);
    }
    let scale = t.scale.x.abs();
    let reach = chain.reach.unwrap_or_else(|| {
        chain.joints[1..]
            .iter()
            .filter_map(|&j| chain.skeleton.bind_pose.get(j as usize))
            .map(|bind| bind.translation.length())
            .sum::<f32>()
            * scale
    });
    if reach.is_finite() && reach > 0.0 {
        stats.reach_spheres += 1;
        for normal in [Vec3::X, Vec3::Y, Vec3::Z] {
            ring(buffer, root_at, normal, reach, REACH_SEGMENTS, colors.reach);
        }
    }
    for (i, pair) in chain.joints.windows(2).enumerate() {
        let limit = chain.limits.get(i).copied().unwrap_or_default();
        if limit == JointLimit::Free {
            continue;
        }
        let (joint, child) = (pair[0], pair[1]);
        let (Some(apex), Some(end)) = (world(joint), world(child)) else {
            continue;
        };
        let length = apex.distance(end);
        let Some(bind) = chain.skeleton.bind_pose.get(joint as usize) else {
            continue;
        };
        let bone_axis = chain
            .skeleton
            .bind_pose
            .get(child as usize)
            .map_or(Vec3::Y, |b| b.translation.normalize_or(Vec3::Y));
        let parent = chain
            .skeleton
            .parent_of(joint as usize)
            .and_then(|p| chain.globals.get(p))
            .map_or(Quat::IDENTITY, rotation_of);
        // The joint's bind frame in the world: limits are measured from it.
        let frame = (t.rotation * parent * bind.rotation).normalize();
        let rest = (frame * bone_axis).normalize_or(Vec3::Y);
        match limit {
            JointLimit::Free => {}
            JointLimit::Cone { max_swing, .. } => {
                stats.cone_limits += 1;
                let swing = if max_swing.is_finite() {
                    max_swing.clamp(0.0, std::f32::consts::PI)
                } else {
                    std::f32::consts::PI
                };
                let (sin, cos) = det_sin_cos(swing);
                let center = apex + rest * (length * cos);
                let (u, v) = rest.any_orthonormal_pair();
                for k in 0..CONE_RAYS {
                    let (s, c) = det_sin_cos(TAU * k as f32 / CONE_RAYS as f32);
                    buffer.line(apex, center + (u * c + v * s) * (length * sin), colors.limit);
                }
                ring(buffer, center, rest, length * sin, RING_SEGMENTS, colors.limit);
            }
            JointLimit::Hinge { axis, min, max } => {
                stats.hinge_limits += 1;
                let axis = (frame * axis.normalize_or(Vec3::X)).normalize_or(Vec3::X);
                let (lo, hi) = (
                    if min.is_finite() {
                        min
                    } else {
                        -std::f32::consts::PI
                    },
                    if max.is_finite() {
                        max
                    } else {
                        std::f32::consts::PI
                    },
                );
                let (lo, hi) = if lo <= hi { (lo, hi) } else { (hi, lo) };
                let arm = length * 0.5;
                let at = |angle: f32| apex + det_quat_from_axis_angle(axis, angle) * rest * arm;
                buffer.linestrip(
                    (0..=HINGE_SEGMENTS).map(|k| at(lo + (hi - lo) * k as f32 / HINGE_SEGMENTS as f32)),
                    colors.limit,
                );
                buffer.line(apex, at(lo), colors.limit);
                buffer.line(apex, at(hi), colors.limit);
            }
        }
    }
}

/// Inputs of one foot placement rig's draw.
pub struct FootPlacementDraw<'a> {
    /// The rig entity (the probe ignores it unless the placement names a character).
    pub entity: Entity,
    /// The component.
    pub placement: &'a FootPlacement,
    /// Its last report, when inserted.
    pub report: Option<&'a FootPlantReport>,
    /// The rig's skeleton.
    pub skeleton: &'a Skeleton,
    /// The rig's joint globals (object space).
    pub globals: &'a [Mat4],
    /// The rig's world placement.
    pub transform: &'a Transform,
    /// The scene probe the overlay casts with.
    pub probe: Option<&'a GroundProbes>,
}

/// Draws one foot placement rig: probe rays and hits with normals, pins with their lock bars,
/// tilted soles, slide rings and the pelvis offset (with `feet`), and each leg's IK chain, target
/// and pole (with `ik`).
pub fn draw_foot_placement<G: GizmoConfigGroup>(
    buffer: &mut GizmoBuffer<G>,
    rig: &FootPlacementDraw<'_>,
    feet: bool,
    ik: bool,
    colors: &LocomotionGizmoColors,
    stats: &mut LocomotionGizmoStats,
) {
    if feet && over_budget(buffer, stats) {
        return;
    }
    let t = rig.transform;
    let placement = rig.placement;
    let plan = placement.applied_plan();
    let height = if placement.probe_height.is_finite() {
        placement.probe_height.abs()
    } else {
        0.6
    };
    let length = if placement.probe_length.is_finite() {
        placement.probe_length.abs()
    } else {
        1.2
    };
    let ignore = placement.character.unwrap_or(rig.entity);
    let scale = t.scale.x.abs();
    if feet {
        stats.planters += 1;
    }
    for leg in 0..placement.legs.len() {
        let Some(joints) = placement.resolved_leg(leg) else {
            continue;
        };
        let Some(ankle) = joint_world(t, rig.globals, joints.ankle) else {
            continue;
        };
        let pin = placement.pin(leg);
        if feet {
            let anchor = pin.unwrap_or(ankle);
            let from = anchor + Vec3::Y * height;
            let hit = rig
                .probe
                .and_then(|probe| probe.probe(from, Vec3::NEG_Y, length, ignore))
                .filter(|hit| hit.point.is_finite() && hit.normal.is_finite());
            stats.probes += 1;
            buffer.line(
                from,
                hit.map_or(from + Vec3::NEG_Y * length, |hit| hit.point),
                colors.probe,
            );
            let report = rig.report;
            if let Some(hit) = hit {
                stats.probe_hits += 1;
                cross(buffer, hit.point, MARKER_SIZE, colors.probe_hit);
                let normal = hit.normal.normalize_or(Vec3::Y);
                buffer.line(hit.point, hit.point + normal * NORMAL_LENGTH, colors.normal);
                if let Some(tilt) = report.and_then(|r| r.tilt_deg.get(leg)).copied() {
                    stats.soles += 1;
                    let axis = Vec3::Y.cross(normal).normalize_or_zero();
                    let sole = if axis == Vec3::ZERO || !tilt.is_finite() {
                        Vec3::Y
                    } else {
                        det_quat_from_axis_angle(axis, tilt.to_radians()) * Vec3::Y
                    };
                    ring(buffer, hit.point, sole, SOLE_RADIUS, RING_SEGMENTS, colors.sole);
                }
            }
            if let Some(pin) = pin {
                stats.pins += 1;
                let lock = report.and_then(|r| r.lock.get(leg)).copied().unwrap_or(0.0);
                ring(buffer, pin, Vec3::Y, PIN_RADIUS, RING_SEGMENTS, colors.pin);
                buffer.line(
                    pin,
                    pin + Vec3::Y * (LOCK_BAR_LENGTH * lock.clamp(0.0, 1.0)),
                    colors.pin,
                );
            }
            let slide = report.and_then(|r| r.slide.get(leg)).copied().unwrap_or(0.0);
            if slide.is_finite() && slide > SLIDE_EPSILON {
                stats.slides += 1;
                let at = hit.map_or(anchor, |hit| hit.point);
                ring(buffer, at, Vec3::Y, slide, RING_SEGMENTS, colors.slide);
            }
        }
        if ik {
            let target = plan.legs.get(leg).map(|out| to_world(t, out.target)).or(pin);
            let pole_local = if placement.is_pose() {
                placement.pose_pole()
            } else {
                rig.globals
                    .get(joints.knee as usize)
                    .map_or(Vec3::ZERO, |m| m.w_axis.truncate())
                    + joints.pole_offset
            };
            draw_ik_chain(
                buffer,
                &IkChainDraw {
                    skeleton: rig.skeleton,
                    globals: rig.globals,
                    transform: t,
                    joints: &[joints.hip, joints.knee, joints.ankle],
                    limits: &[],
                    target,
                    pole: Some(to_world(t, pole_local)),
                    reach: Some(joints.length * scale),
                },
                colors,
                stats,
            );
        }
    }
    if feet && let Some(report) = rig.report {
        let root = rig.skeleton.parents.iter().position(Option::is_none);
        if let Some(root) = root.and_then(|r| joint_world(t, rig.globals, r as u32))
            && report.pelvis_offset.is_finite()
        {
            stats.pelvis_markers += 1;
            cross(buffer, root, MARKER_SIZE, colors.pelvis);
            buffer.line(root, root - Vec3::Y * report.pelvis_offset, colors.pelvis);
        }
    }
}

/// Inputs of one gait rig's IK draw.
pub struct GaitRigDraw<'a> {
    /// The component (its resolved legs).
    pub rig: &'a GaitRig,
    /// Its gait state (the foot targets).
    pub state: &'a GaitState,
    /// The rig's skeleton.
    pub skeleton: &'a Skeleton,
    /// The rig's joint globals (object space).
    pub globals: &'a [Mat4],
    /// The rig's world placement.
    pub transform: &'a Transform,
}

/// Draws the legs of a [`GaitRig`] as IK chains, each targeting its gait foot target raised by
/// the leg's bind tip height.
pub fn draw_gait_rig_ik<G: GizmoConfigGroup>(
    buffer: &mut GizmoBuffer<G>,
    walker: &GaitRigDraw<'_>,
    colors: &LocomotionGizmoColors,
    stats: &mut LocomotionGizmoStats,
) {
    let transform = walker.transform;
    let scale = transform.scale.x;
    for leg in 0..walker.state.leg_count() {
        let Some(chain) = walker.rig.leg_chain(leg) else {
            continue;
        };
        let lift = walker.rig.leg_lift(leg).unwrap_or(0.0);
        let target = v3(walker.state.feet[leg].position) + transform.rotation * Vec3::Y * (lift * scale);
        let two_bone;
        let (joints, limits): (&[u32], &[JointLimit]) = match chain {
            GaitLegChain::TwoBone { root, mid, tip } => {
                two_bone = [root, mid, tip];
                (&two_bone, &[])
            }
            GaitLegChain::Chain { joints, limits } => (joints, limits),
        };
        draw_ik_chain(
            buffer,
            &IkChainDraw {
                skeleton: walker.skeleton,
                globals: walker.globals,
                transform,
                joints,
                limits,
                target: Some(target),
                pole: None,
                reach: None,
            },
            colors,
            stats,
        );
    }
}

/// The overlay's walkers: a gait rig with its state.
type Walkers<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static GaitRig,
        &'static GaitState,
        &'static SkinnedMesh,
        Option<&'static JointGlobals>,
        &'static Transform,
    ),
    Without<regia_scene::layers::LayerParked>,
>;

/// The overlay's foot planters.
type Planters<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static FootPlacement,
        Option<&'static FootPlantReport>,
        &'static SkinnedMesh,
        &'static JointGlobals,
        &'static Transform,
    ),
    Without<regia_scene::layers::LayerParked>,
>;

/// Draws every walker and foot planter (`PostUpdate`, only while the group is enabled).
pub fn draw_locomotion(
    mut gizmos: Gizmos<LocomotionGizmos>,
    colors: Res<LocomotionGizmoColors>,
    mut stats: ResMut<LocomotionGizmoStats>,
    probes: Option<Res<GroundProbes>>,
    walkers: Walkers,
    planters: Planters,
    mut refused_seen: Local<RefusedSeen>,
) {
    let _span = tracing::debug_span!(target: "regia_framework", "locomotion_gizmos").entered();
    let toggles = gizmos.config_ext().copied().unwrap_or_default();
    let stats = &mut *stats;
    stats.reset_frame();
    let colors = &*colors;
    let frame = stats.frames;
    for (entity, rig, state, mesh, globals, transform) in &walkers {
        if toggles.gait {
            // Observed for every walker, drawn or over the budget, so a walker that comes back
            // under the budget does not flash a stale refusal.
            let refused_now = refused_seen.observe(entity, state.refused, frame);
            let attitude = rig
                .body_joint()
                .and_then(|b| globals.and_then(|g| g.globals.get(b as usize)))
                .map(|m| (transform.rotation * rotation_of(m)).normalize());
            draw_gait(
                &mut gizmos,
                &rig.params,
                state,
                attitude,
                refused_now,
                colors,
                stats,
            );
        }
        if toggles.ik
            && let Some(globals) = globals
        {
            draw_gait_rig_ik(
                &mut gizmos,
                &GaitRigDraw {
                    rig,
                    state,
                    skeleton: &mesh.skeleton,
                    globals: &globals.globals,
                    transform,
                },
                colors,
                stats,
            );
        }
    }
    refused_seen.retire(frame);
    if toggles.foot_placement || toggles.ik {
        for (entity, placement, report, mesh, globals, transform) in &planters {
            draw_foot_placement(
                &mut gizmos,
                &FootPlacementDraw {
                    entity,
                    placement,
                    report,
                    skeleton: &mesh.skeleton,
                    globals: &globals.globals,
                    transform,
                    probe: probes.as_deref(),
                },
                toggles.foot_placement,
                toggles.ik,
                colors,
                stats,
            );
        }
    }
    let view = gizmos.buffer();
    stats.lines = (view.list_positions.len() / 2) as u32;
    stats.strips = view.strip_positions.iter().filter(|p| p.x.is_nan()).count() as u32;
    stats.strip_vertices = view.strip_positions.len() as u32 - stats.strips;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cost-gizmos review finding 4: the refused-count list keeps only the walkers of the
    /// latest drawn frame. 100 walkers, then 100 other walkers, then 100 more: 100 entries
    /// each time, and once the first swap has sized it the list's capacity stays put (it grew
    /// by 100 entries per swap before).
    #[test]
    fn refused_seen_drops_despawned_walkers() {
        let entity = |i: u32| Entity::from_raw_u32(i + 1).expect("entity");
        let mut seen = RefusedSeen::default();
        for i in 0..100_u32 {
            assert!(!seen.observe(entity(i), 0, 1));
        }
        seen.retire(1);
        assert_eq!(seen.entries.len(), 100);
        let mut capacity = 0;
        for round in 2..6_u32 {
            for i in 0..100_u32 {
                assert!(!seen.observe(entity(round * 100 + i), 0, u64::from(round)));
            }
            seen.retire(u64::from(round));
            assert_eq!(
                seen.entries.len(),
                100,
                "round {round}: despawned walkers were kept"
            );
            if round == 2 {
                capacity = seen.entries.capacity();
            }
            assert_eq!(
                seen.entries.capacity(),
                capacity,
                "round {round}: the list kept growing"
            );
        }
        let walker = entity(550);
        assert!(seen.observe(walker, 1, 6), "a grown count marks the body");
        assert!(!seen.observe(walker, 1, 6), "an unchanged count does not");
        seen.retire(6);
        assert_eq!(seen.entries.len(), 1);
    }
}
