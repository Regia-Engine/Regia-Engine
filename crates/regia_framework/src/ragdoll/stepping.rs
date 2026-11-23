// SPDX-License-Identifier: MPL-2.0
//! Recovery beyond standing for active ragdolls (`docs/specs/93-character-physics.md` § 10, ADR-2963): capture-point
//! stepping, landing from a drop, and planting feet on bodies that move.
//!
//! All three are off by default ([`RagdollRecovery::default`]), so an existing profile behaves bit for bit as the
//! standing-only controller of ADR-2564. [`RagdollRecovery::full`] turns them on with the tuning the tests use.
//!
//! **Stepping** (after Pratt, Carff, Drakunov and Goswami 2006, "Capture Point: A Step toward Humanoid Push Recovery"):
//! the instantaneous capture point `c + v sqrt(h / g)` is where a linear inverted pendulum of the center of mass's
//! height would have to put its support to come to rest. When it leaves the support polygon by more than
//! `trigger_margin`, the controller picks a swing foot (the foot on the side the capture point left towards; for a
//! push straight ahead or back, the foot behind), and a step target: the capture point plus the swing foot's authored
//! offset from the center of mass across the direction of travel, plus `overshoot` along it, clamped to `max_step`
//! from the stance foot and kept `min_width` clear of it sideways; the capture point used is the one predicted for the
//! touch-down (`p + (c - p) exp(omega t)`, `p` the polygon point nearest to it, `omega = sqrt(g / h)`), and the foot's sole
//! centroid, not its origin, goes to the target, pulled onto the disk the leg can reach from its hip once the stance
//! hip has dropped as far as it may (a target past it leaves the foot hovering while the body falls). The swing foot
//! is released and driven along a
//! fixed-step timeline (`swing_time`): lifted by `lift` on a half sine while it moves on a smoothstep, then pressed
//! down until it touches. Its leg's targets come from a two-bone inverse kinematics solve in world space, turned into
//! joint-space targets like every other motor (the hip relative to the pelvis' current rotation, the knee relative to
//! the thigh's target, the foot flat). Touching down after half the swing plants the foot; the controller then checks
//! the capture point against the new polygon and steps again if it is still outside, at most `max_steps` times in one
//! recovery. Past that the standing controller's fall test decides. After a step the legs stand in the stance the
//! steps left (inverse kinematics from where the pelvis should be over the feet to the planted feet) instead of the
//! authored stance, so the hips do not fight the new foot layout.
//!
//! **Landing** (original design): a ragdoll that is `Active` with no foot touching anything is airborne. It does not
//! count towards a fall (for up to `max_air_time`); its legs reach for the capture point with the knees bent
//! (`pre_bend` of the leg's length taken off), the authored foot layout around it. The first touch starts the
//! absorb: the touching feet plant and the legs hold a crouch `crouch` deep that rises back to the standing height
//! over `absorb_time`. Then it stands.
//!
//! **Feet on bodies that move** (original design): a foot whose contacts all belong to one body that can move (a
//! crate, a lift) is planted *in that body's frame*: it stays locked (the solver sees an infinitely heavy anchor, as on
//! fixed ground, with the same sole torque limits), and each step its velocity is set to the velocity of the support
//! at the foot plus a correction back onto the pose it had in the support's frame when it planted. A dynamic support
//! also receives the ragdoll's weight share at the foot each step, since a locked foot carries no weight into it. The
//! capture point uses the center of mass's velocity relative to the support.
//!
//! **Damping and solver iterations** (ADR-2963): while a recovery is under way (a swing, airborne, landing, or the
//! stance a step or landing left) the joints that carry the body get three times their authored damping; the swinging
//! leg keeps its own. Stepping and landing need more solver iterations than standing: at the default 4 a single-support
//! phase does not converge and outcomes flip on small changes; both drops land standing from 24 iterations and sideways
//! shoves are recovered at 32 (`PhysicsSettings::solver_iterations`). Moving supports hold at the default 4.

use regia_core::math::{Quat, Vec2, Vec3};

use crate::detmath;

/// Recovery beyond standing (module docs): stepping, landing and planting on moving bodies. Everything is off by
/// default.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RagdollRecovery {
    /// Largest number of steps in one recovery (0: never step, the standing-only controller).
    pub max_steps: u32,
    /// Distance (m) the capture point may leave the support polygon before a step starts.
    pub trigger_margin: f32,
    /// Distance (m) the step lands beyond the capture point, along the direction of travel.
    pub overshoot: f32,
    /// Longest step (m), from the stance foot to the swing foot, horizontally.
    pub max_step: f32,
    /// Least sideways distance (m) between the stance foot and the swing foot's target (no crossed legs).
    pub min_width: f32,
    /// Seconds (counted in fixed steps) a swing takes from lift-off to the planned touch-down.
    pub swing_time: f32,
    /// Height (m) the swing foot is lifted at mid-swing.
    pub lift: f32,
    /// Land on the feet when switched to `Active` in the air (or knocked off the ground).
    pub landing: bool,
    /// Knee bend while airborne: the fraction of the leg's length taken off the hip-to-ankle distance.
    pub pre_bend: f32,
    /// Depth (m) of the crouch that absorbs a landing.
    pub crouch: f32,
    /// Seconds (counted in fixed steps) the crouch takes to rise back to the standing height.
    pub absorb_time: f32,
    /// Seconds (counted in fixed steps) airborne before the ragdoll gives up and falls.
    pub max_air_time: f32,
    /// Plant feet on a single body that can move, in its frame (off: such a foot stays free, ADR-2564).
    pub moving_support: bool,
}

impl Default for RagdollRecovery {
    /// Everything off (the standing-only controller); the tuning numbers are `full()`'s, for when a part is turned on.
    fn default() -> Self {
        RagdollRecovery {
            max_steps: 0,
            trigger_margin: 0.05,
            overshoot: 0.05,
            max_step: 0.6,
            min_width: 0.14,
            swing_time: 0.3,
            lift: 0.08,
            landing: false,
            pre_bend: 0.08,
            crouch: 0.08,
            absorb_time: 0.5,
            max_air_time: 2.0,
            moving_support: false,
        }
    }
}

impl RagdollRecovery {
    /// Stepping (3 steps), landing and planting on moving bodies all on, with the tuning the tests use.
    pub fn full() -> Self {
        RagdollRecovery {
            max_steps: 3,
            trigger_margin: 0.05,
            overshoot: 0.05,
            max_step: 0.6,
            min_width: 0.14,
            swing_time: 0.3,
            lift: 0.08,
            landing: true,
            pre_bend: 0.08,
            crouch: 0.08,
            absorb_time: 0.5,
            max_air_time: 2.0,
            moving_support: true,
        }
    }

    /// Whether any part is on.
    pub fn any(&self) -> bool {
        self.max_steps > 0 || self.landing || self.moving_support
    }

    /// Whether every number is finite and in range: lengths and times non-negative, `pre_bend` in `0..=0.5`.
    pub fn valid(&self) -> bool {
        let lengths = [
            self.trigger_margin,
            self.overshoot,
            self.max_step,
            self.min_width,
            self.swing_time,
            self.lift,
            self.crouch,
            self.absorb_time,
            self.max_air_time,
        ];
        lengths.iter().all(|v| v.is_finite() && *v >= 0.0)
            && self.pre_bend.is_finite()
            && (0.0..=0.5).contains(&self.pre_bend)
            && self.max_steps <= 16
    }
}

/// What the recovery layer is doing (`ragdoll.state` `step_phase`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RagdollStepPhase {
    /// Standing on the planted feet (also every ragdoll with recovery off).
    #[default]
    Stand,
    /// A foot is swinging to a step target.
    Swing,
    /// No foot touches anything; the legs reach for the ground.
    Airborne,
    /// Just landed: the legs absorb the impact in a crouch and rise.
    Landing,
}

impl RagdollStepPhase {
    /// The phase's protocol name (`stand`, `swing`, `airborne`, `landing`).
    pub fn name(self) -> &'static str {
        match self {
            RagdollStepPhase::Stand => "stand",
            RagdollStepPhase::Swing => "swing",
            RagdollStepPhase::Airborne => "airborne",
            RagdollStepPhase::Landing => "landing",
        }
    }
}

/// Why an active ragdoll fell (`ragdoll.state` `fall_cause`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RagdollFallCause {
    /// The capture point stayed outside the support polygon (and no step was left, or stepping is off).
    Capture,
    /// No foot touched anything for too long.
    NoContact,
    /// The center of mass sank below 0.7 of its animated height over the ankles.
    Collapse,
    /// A swing foot did not touch down in time.
    MissedStep,
}

impl RagdollFallCause {
    /// The cause's protocol name (`capture`, `no_contact`, `collapse`, `missed_step`).
    pub fn name(self) -> &'static str {
        match self {
            RagdollFallCause::Capture => "capture",
            RagdollFallCause::NoContact => "no_contact",
            RagdollFallCause::Collapse => "collapse",
            RagdollFallCause::MissedStep => "missed_step",
        }
    }
}

/// One leg of the rig: body indices of the thigh (its parent is the pelvis side), the shin and the foot (the `Ankle`
/// motor's body), and the animated lengths of the thigh and the shin.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Leg {
    pub thigh: usize,
    pub shin: usize,
    pub foot: usize,
    pub upper: f32,
    pub lower: f32,
}

/// A swing in progress.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Swing {
    /// Index into the rig's legs.
    pub leg: usize,
    /// Fixed steps since lift-off.
    pub elapsed: u32,
    /// Where the swing foot's origin was at lift-off.
    pub start: Vec3,
    /// Where it should touch down.
    pub target: Vec3,
    /// The centroid of the foot's contacts at lift-off in the foot's frame: the step puts this point, not the foot's
    /// origin (often the heel), at the planned support point.
    pub sole: Vec3,
}

/// Two-bone inverse kinematics: the knee and ankle positions for a hip at `hip`, bones of length `upper` and `lower`,
/// the ankle wanted at `target` (moved onto the reachable shell when it is too far or too near) and the knee bending
/// towards `pole`. `None` for degenerate input (non-finite, zero lengths, target on the hip).
pub fn two_bone_ik(hip: Vec3, upper: f32, lower: f32, target: Vec3, pole: Vec3) -> Option<(Vec3, Vec3)> {
    let usable = |length: f32| length.is_finite() && length > 1e-4;
    if !(hip.is_finite() && target.is_finite() && pole.is_finite() && usable(upper) && usable(lower)) {
        return None;
    }
    let to = target - hip;
    let distance = to.length();
    if distance.is_nan() || distance <= 1e-5 {
        return None;
    }
    let dir = to / distance;
    let reach = (upper + lower) * 0.9999;
    let near = (upper - lower).abs() + 1e-4;
    let d = distance.clamp(near.min(reach), reach);
    let cos = ((upper * upper + d * d - lower * lower) / (2.0 * upper * d)).clamp(-1.0, 1.0);
    let sin = (1.0 - cos * cos).max(0.0).sqrt();
    let mut bend = (pole - dir * pole.dot(dir)).normalize_or_zero();
    if bend == Vec3::ZERO {
        bend = dir.any_orthonormal_vector();
    }
    let knee = hip + (dir * cos + bend * sin) * upper;
    Some((knee, hip + dir * d))
}

/// `base` turned by the shortest arc that takes the world direction `from` onto `to`; `base` itself when either is
/// zero or not finite.
pub(super) fn aim(base: Quat, from: Vec3, to: Vec3) -> Quat {
    let (a, b) = (from.normalize_or_zero(), to.normalize_or_zero());
    if a == Vec3::ZERO || b == Vec3::ZERO {
        return base;
    }
    let arc = Quat::from_rotation_arc(a, b);
    if arc.is_finite() {
        (arc * base).normalize()
    } else {
        base
    }
}

/// World rotations of a leg's thigh and shin that put the knee at `knee` and the ankle at `ankle` from `hip`, keeping
/// the animated twist: each animated rotation turned by the arc from its animated bone direction.
pub(super) fn leg_rotations(
    animated: (Quat, Quat),
    animated_axes: (Vec3, Vec3),
    hip: Vec3,
    knee: Vec3,
    ankle: Vec3,
) -> (Quat, Quat) {
    (
        aim(animated.0, animated_axes.0, knee - hip),
        aim(animated.1, animated_axes.1, ankle - knee),
    )
}

/// The swing foot's wanted origin `s` (0 at lift-off, 1 at the planned touch-down, beyond 1 pressing down) along the
/// timeline: a smoothstep from `start` to `target` horizontally, a half sine of height `lift` over the straight line
/// between their heights, and past the end `press` metres below the target.
pub fn swing_point(start: Vec3, target: Vec3, s: f32, lift: f32, press: f32) -> Vec3 {
    let t = s.clamp(0.0, 1.0);
    let ease = t * t * (3.0 - 2.0 * t);
    let mut p = start.lerp(target, ease);
    p.y = start.y + (target.y - start.y) * t + lift * detmath::sin_cos(std::f32::consts::PI * t).0;
    if s >= 1.0 {
        p.y = target.y - press;
    }
    p
}

/// `target` (a swing foot's wanted origin) pulled onto the disk a leg of bones `upper` and `lower` can reach from a hip
/// at `hip` once that hip has come down by `drop`: horizontally at most `sqrt(L^2 - h^2)` from the hip, `L` 97 % of
/// the leg's length and `h` the lowered hip's height over the target. The target's height is kept. Non-finite input
/// comes back unchanged (the callers check targets for finiteness).
pub fn within_reach(hip: Vec3, upper: f32, lower: f32, drop: f32, target: Vec3) -> Vec3 {
    let length = (upper + lower) * 0.97;
    let height = (hip.y - drop - target.y).max(0.0);
    let radius = (length * length - height * height).max(0.0).sqrt();
    let offset = Vec3::new(target.x - hip.x, 0.0, target.z - hip.z);
    let distance = offset.length();
    if distance > radius && distance > 0.0 {
        let clamped = hip + offset * (radius / distance);
        Vec3::new(clamped.x, target.y, clamped.z)
    } else {
        target
    }
}

/// The step target (module docs). `capture` is the capture point, `com` the center of mass (both on the ground plane,
/// x/z), `offset` the swing foot's authored horizontal offset from the center of mass, `stance` the stance foot's
/// origin (x/z), `side` the unit horizontal direction from the stance foot towards the swing foot's side. Returns the
/// x/z of the swing foot's origin at touch-down.
pub fn step_target(
    capture: Vec2,
    com: Vec2,
    offset: Vec2,
    stance: Vec2,
    side: Vec2,
    recovery: &RagdollRecovery,
) -> Vec2 {
    let travel = (capture - com).normalize_or_zero();
    // The authored offset across the direction of travel only: along it the step goes to the capture point itself.
    let across = if travel == Vec2::ZERO {
        offset
    } else {
        offset - travel * offset.dot(travel)
    };
    let mut target = capture + across + travel * recovery.overshoot;
    // No crossed legs: at least `min_width` to the swing foot's side of the stance foot.
    let sideways = (target - stance).dot(side);
    if sideways < recovery.min_width {
        target += side * (recovery.min_width - sideways);
    }
    // Leg reach.
    let from = target - stance;
    let length = from.length();
    if length > recovery.max_step && length > 0.0 {
        target = stance + from * (recovery.max_step / length);
        // The radial clamp keeps the direction from the stance foot, which can bring a long step
        // close to the travel axis back inside `min_width`: keep the width and spend what is left
        // of the reach along the step instead (the max-step disk cut by the no-crossing line).
        let sideways = (target - stance).dot(side);
        if sideways < recovery.min_width {
            let along = (from - side * from.dot(side)).normalize_or_zero();
            let width = recovery.min_width.min(recovery.max_step);
            let room = (recovery.max_step * recovery.max_step - width * width)
                .max(0.0)
                .sqrt();
            target = stance + side * recovery.min_width + along * room;
        }
    }
    if target.is_finite() {
        target
    } else {
        stance + side * recovery.min_width
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_bone_ik_reaches_bends_forward_and_clamps() {
        let hip = Vec3::new(0.0, 1.0, 0.0);
        let (knee, ankle) = two_bone_ik(hip, 0.5, 0.5, Vec3::new(0.0, 0.2, 0.0), Vec3::NEG_Z).expect("solve");
        assert!((ankle - Vec3::new(0.0, 0.2, 0.0)).length() < 1e-5);
        assert!(((knee - hip).length() - 0.5).abs() < 1e-5);
        assert!(((ankle - knee).length() - 0.5).abs() < 1e-5);
        assert!(knee.z < -0.1, "the knee bends towards the pole: {knee}");
        // Out of reach: a straight leg pointing at the target.
        let (knee, ankle) =
            two_bone_ik(hip, 0.5, 0.5, Vec3::new(0.0, -2.0, 0.0), Vec3::NEG_Z).expect("solve");
        assert!(
            (ankle.y - 0.0).abs() < 1e-3 && knee.z.abs() < 0.02,
            "{knee} {ankle}"
        );
        assert!(two_bone_ik(hip, 0.5, 0.5, hip, Vec3::Z).is_none());
        assert!(two_bone_ik(hip, 0.0, 0.5, Vec3::ZERO, Vec3::Z).is_none());
        assert!(two_bone_ik(hip, 0.5, 0.5, Vec3::splat(f32::NAN), Vec3::Z).is_none());
        // A pole along the leg still bends somewhere.
        assert!(two_bone_ik(hip, 0.5, 0.5, Vec3::new(0.0, 0.3, 0.0), Vec3::Y).is_some());
    }

    #[test]
    fn the_swing_lifts_then_presses_down() {
        let start = Vec3::new(0.0, 0.05, 0.0);
        let target = Vec3::new(0.0, 0.05, -0.3);
        assert_eq!(swing_point(start, target, 0.0, 0.08, 0.03), start);
        let mid = swing_point(start, target, 0.5, 0.08, 0.03);
        assert!(
            (mid.y - 0.13).abs() < 1e-5 && (mid.z + 0.15).abs() < 1e-5,
            "{mid}"
        );
        let end = swing_point(start, target, 1.2, 0.08, 0.03);
        assert!((end.z + 0.3).abs() < 1e-5 && (end.y - 0.02).abs() < 1e-5, "{end}");
    }

    fn reference_sin(radians: f32) -> f32 {
        let q = f64::from(radians) / std::f64::consts::FRAC_PI_2;
        let k = (q + 0.5).floor();
        let x = (q - k) * std::f64::consts::FRAC_PI_2;
        let x2 = x * x;
        let (mut sin, mut cos) = (0.0, 0.0);
        for n in (0..8).rev() {
            let m = f64::from(2 * n);
            cos = 1.0 - cos * x2 / ((m + 1.0) * (m + 2.0));
            sin = 1.0 - sin * x2 / ((m + 2.0) * (m + 3.0));
        }
        let sin = x * sin;
        let value = match (k as i64).rem_euclid(4) {
            0 => sin,
            1 => cos,
            2 => -sin,
            _ => -cos,
        };
        value as f32
    }

    #[test]
    fn the_swing_lift_uses_the_deterministic_sine_bits() {
        let start = Vec3::new(0.0, 0.05, 0.0);
        let target = Vec3::new(0.0, 0.05, -0.3);
        for i in 1..10_000 {
            let s = i as f32 * 0.0001;
            let expected =
                start.y + (target.y - start.y) * s + 0.08 * reference_sin(std::f32::consts::PI * s);
            assert_eq!(
                swing_point(start, target, s, 0.08, 0.03).y.to_bits(),
                expected.to_bits(),
                "s = {s}"
            );
        }
    }

    #[test]
    fn the_step_target_follows_the_capture_point_without_crossing() {
        let r = RagdollRecovery::full();
        // Pushed forward (-Z), the right foot (x = -0.1) steps: lands ahead of the capture point, on its own side.
        let t = step_target(
            Vec2::new(0.0, -0.3),
            Vec2::ZERO,
            Vec2::new(-0.1, 0.0),
            Vec2::new(0.1, 0.0),
            Vec2::new(-1.0, 0.0),
            &r,
        );
        assert!((t.y + 0.35).abs() < 1e-5, "{t}");
        assert!(t.x <= 0.1 - r.min_width + 1e-5, "{t}");
        // Pushed right (+X... the left side is +X here): a capture point beyond the stance foot is clamped to its side.
        let t = step_target(
            Vec2::new(0.4, 0.0),
            Vec2::ZERO,
            Vec2::new(0.1, 0.0),
            Vec2::new(-0.1, 0.0),
            Vec2::new(1.0, 0.0),
            &r,
        );
        assert!(
            t.x > 0.4 && (t - Vec2::new(-0.1, 0.0)).length() <= r.max_step + 1e-5,
            "{t}"
        );
        // Far away: clamped to the longest step.
        let t = step_target(
            Vec2::new(0.0, -3.0),
            Vec2::ZERO,
            Vec2::new(-0.1, 0.0),
            Vec2::new(0.1, 0.0),
            Vec2::new(-1.0, 0.0),
            &r,
        );
        assert!(
            ((t - Vec2::new(0.1, 0.0)).length() - r.max_step).abs() < 1e-4,
            "{t}"
        );
        // NaN input never panics and gives a finite point.
        let t = step_target(
            Vec2::splat(f32::NAN),
            Vec2::ZERO,
            Vec2::ZERO,
            Vec2::ZERO,
            Vec2::X,
            &r,
        );
        assert!(t.is_finite());
    }

    /// rc1 physics review, finding 6: a long step whose direction from the stance foot is close to
    /// the travel axis came out of the `max_step` clamp narrower than `min_width` (crossed legs).
    /// It keeps the width now and still reaches no further than `max_step`.
    #[test]
    fn a_long_step_keeps_the_minimum_width() {
        let r = RagdollRecovery::full();
        let stance = Vec2::new(0.1, 0.0);
        let side = Vec2::new(-1.0, 0.0);
        for capture_z in [-0.7, -1.0, -3.0, -10.0] {
            let t = step_target(
                Vec2::new(0.0, capture_z),
                Vec2::ZERO,
                Vec2::new(-0.1, 0.0),
                stance,
                side,
                &r,
            );
            let sideways = (t - stance).dot(side);
            assert!(
                sideways >= r.min_width - 1e-5,
                "capture z {capture_z}: {t}, sideways {sideways}"
            );
            assert!(
                (t - stance).length() <= r.max_step + 1e-5,
                "capture z {capture_z}: {t}"
            );
            assert!(t.y < -0.5, "the step still goes forward: {t}");
        }
    }

    #[test]
    fn a_step_target_out_of_reach_is_pulled_in() {
        // Hip 0.9 m up, legs 0.42 + 0.43 m, 5 cm of drop: the foot can reach about 0.2 m out horizontally.
        let hip = Vec3::new(0.1, 0.9, 0.0);
        let near = Vec3::new(0.2, 0.05, -0.1);
        assert_eq!(within_reach(hip, 0.42, 0.43, 0.05, near), near);
        let far = Vec3::new(0.8, 0.05, 0.0);
        let pulled = within_reach(hip, 0.42, 0.43, 0.05, far);
        let length = 0.85 * 0.97_f32;
        let radius = (length * length - 0.8_f32 * 0.8).sqrt();
        assert!(
            (pulled.x - (0.1 + radius)).abs() < 1e-5 && pulled.z.abs() < 1e-6,
            "{pulled}"
        );
        assert_eq!(pulled.y, far.y);
        // A hip too high for the leg to reach the ground: the target comes in to straight below the hip.
        let high = within_reach(Vec3::new(0.0, 2.0, 0.0), 0.42, 0.43, 0.05, far);
        assert!(high.x.abs() < 1e-6 && high.z.abs() < 1e-6, "{high}");
        // NaN never panics.
        let _ = within_reach(Vec3::splat(f32::NAN), 0.42, 0.43, 0.05, far);
        let _ = within_reach(hip, f32::NAN, 0.43, 0.05, Vec3::splat(f32::NAN));
    }

    #[test]
    fn defaults_are_off_and_full_is_valid() {
        assert!(!RagdollRecovery::default().any());
        let on = RagdollRecovery {
            max_steps: 3,
            landing: true,
            moving_support: true,
            ..RagdollRecovery::default()
        };
        assert_eq!(
            on,
            RagdollRecovery::full(),
            "default and full differ only in what is on"
        );
        assert!(RagdollRecovery::full().any() && RagdollRecovery::full().valid());
        let bad = RagdollRecovery {
            lift: f32::NAN,
            ..RagdollRecovery::full()
        };
        assert!(!bad.valid());
        assert_eq!(RagdollStepPhase::Landing.name(), "landing");
        assert_eq!(RagdollFallCause::MissedStep.name(), "missed_step");
    }
}
