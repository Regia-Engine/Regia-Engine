// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/ai/aim.rs, commit c9513623f, tag regia-absorb-4;
// ir-sim/src/ai/mod.rs lines 320-321, 961-1000 and 1059-1072), ADR-8012.
// Mark: adapt (aim.rs lines 1-441, plan row S-11, ADR-8462). The lead solve, the ballistic arc, the seeker lock,
// the slot choice, the aim error and the fire gate are DO27's code apart from rustfmt, doc comments and these named
// changes:
// - DO27's weapon stem tables (`weapon_role`, `projectile_speed`: game content) are not copied: a slot of the read
//   view carries its `role` and `projectile_speed` (the host fills them from its weapon table) instead of a stem;
// - the read-view types the gate reads (`Stance`, `AmmoSlotView`, `AmmoView`, `AimError`, `FireIntent`; DO27's
//   `mod.rs`) move here; the unit view is `ShooterView`, the four fields of DO27's unit view the gate reads (its
//   position, weapons-free flag, stance and ammo) plus the mortar range setting; `Stance::from_code` (DO27's order
//   book) is not copied;
// - the mortar minimum range (DO27's 90 m constant, `MORTAR_MIN_RANGE_M`) is also a setting
//   (`ShooterView::mortar_min_range_m`, `choose_slot_with`), with DO27's value as the default;
// - the fire gate takes its target as one `FireTarget` (DO27's five target arguments) and its line of sight as any
//   S-10 `LineOfSight` (DO27's `Option<&TerrainSampler>`; `None` is still open field), which also removes DO27's
//   `#[allow(clippy::too_many_arguments)]`;
// - the five float `max` calls go through `regia_detsim::float::det_max` (ADR-8085: the same bits away from a zero
//   or NaN result; lockstep crates deny std `max`); `crate::math` is libm 0.2.16 (DO27's bits);
// - the tests build the slots from the inlined numbers of DO27's tables (a cannon: direct, 120 m/s, 0-150 m; a
//   seeker: 200 m/s, 30-200 m; a mortar: 90 m/s, 90-300 m) and DO27's game names are dropped.

//! Aim and fire.
//!
//! Lead targeting from projectile speed and target velocity, human-like aim error (lead scale, tracking lag,
//! spread — never a damage buff), weapon choice by range band + target type + ammo, the hard deterministic fire
//! gate, ballistic arcs for mortars and the seeker lock rules.
//!
//! Everything is a pure function of the read view; no yaw mutation, so firing never fights steering.
//! Deterministic: `crate::math` only, and the only RNG (spread) is drawn from the per-unit-per-tick [`AiRng`]
//! stream.

use serde::{Deserialize, Serialize};

use super::AiRng;
use super::difficulty::AimParams;
use super::influence::{line_of_fire, los_clear};
use super::los::LineOfSight;
use super::perception::{KnownEnemy, SightingsBoard};
use crate::float::det_max;

/// Beyond this range (metres) an AI unit lobs its mortar at a structure: the default of
/// [`ShooterView::mortar_min_range_m`].
pub const MORTAR_MIN_RANGE_M: f32 = 90.0;

/// A weapon's firing role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WeaponRole {
    /// Straight-firing cannon/beam.
    Direct,
    /// Lobbed ballistic shell (mortar/howitzer/drop rocket).
    Mortar,
    /// Homing missile (seeker); anti-hull only, never vs structures.
    Seeker,
}

/// A unit's fire stance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Stance {
    /// Fire at will.
    #[default]
    Aggressive,
    /// Fire, but hold position.
    Defensive,
    /// Never fire.
    HoldFire,
}

/// One hardpoint slot in the read view. An empty slot is `None` in [`AmmoView::slots`], never assumed.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AmmoSlotView {
    /// The weapon's role.
    pub role: WeaponRole,
    /// Projectile speed, m/s (beams: effectively instantaneous, e.g. 10 000).
    pub projectile_speed: f32,
    /// Per-slot ammo current/max, or `None` for energy weapons / unknown (never dry).
    pub ammo: Option<(u32, u32)>,
    /// Effective `[min, max]` range band, metres.
    pub band: [f32; 2],
}

/// A unit's hardpoints, in slot order.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AmmoView {
    /// One entry per hardpoint; `None` is an empty hardpoint.
    pub slots: Vec<Option<AmmoSlotView>>,
}

/// What the fire gate reads of the shooting unit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShooterView {
    /// World position.
    pub pos: [f32; 3],
    /// The unit may fire (its order allows it).
    pub weapons_free: bool,
    /// The unit's stance; [`Stance::HoldFire`] never fires.
    pub stance: Stance,
    /// The unit's hardpoints.
    pub ammo: AmmoView,
    /// Beyond this range a mortar is chosen against a structure ([`MORTAR_MIN_RANGE_M`] by default).
    pub mortar_min_range_m: f32,
}

impl ShooterView {
    /// A weapons-free, aggressive shooter at `pos` with `ammo` and the default mortar range.
    pub fn new(pos: [f32; 3], ammo: AmmoView) -> Self {
        ShooterView {
            pos,
            weapons_free: true,
            stance: Stance::Aggressive,
            ammo,
            mortar_min_range_m: MORTAR_MIN_RANGE_M,
        }
    }
}

/// What the fire gate reads of the target.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FireTarget {
    /// The target's entity id.
    pub id: u32,
    /// The target's position.
    pub pos: [f32; 3],
    /// The target's velocity.
    pub vel: [f32; 3],
    /// A unit (a seeker may lock it); `false` is a structure.
    pub is_unit: bool,
    /// The range to the target, metres.
    pub range: f32,
}

/// A shot's human-like aim error.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AimError {
    /// The lead-point miss, metres.
    pub lead: f32,
    /// Ticks before the reticle settles.
    pub tracking_lag_ticks: u32,
    /// This shot's spread cone, milliradians.
    pub spread_mrad: f32,
}

/// The fire gate's output: fire `slot` at `target` with `aim_error`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FireIntent {
    /// The hardpoint slot.
    pub slot: usize,
    /// The target's entity id.
    pub target: u32,
    /// The shot's aim error.
    pub aim_error: AimError,
}

/// A slot is empty (no weapon mounted) or has ammo. Energy weapons carry `ammo = None` and never run dry; a
/// `Some((cur, _))` is dry when `cur == 0`.
#[inline]
fn slot_usable(slot: &AmmoSlotView) -> bool {
    match slot.ammo {
        None => true,
        Some((cur, _)) => cur > 0,
    }
}

/// Iterative lead aim point for direct fire: solve the intercept of a projectile at `speed` and a target moving
/// at `vel`, in the XZ plane. Three fixed iterations are deterministic and more than enough to converge.
pub fn lead_point(shooter: [f32; 3], target: [f32; 3], target_vel: [f32; 3], speed: f32) -> [f32; 3] {
    let speed = det_max(speed, 1.0);
    let mut px = target[0];
    let mut pz = target[2];
    for _ in 0..3 {
        let dx = px - shooter[0];
        let dz = pz - shooter[2];
        let dist = crate::math::sqrt(dx * dx + dz * dz);
        let t = dist / speed;
        px = target[0] + target_vel[0] * t;
        pz = target[2] + target_vel[2] * t;
    }
    [px, target[1], pz]
}

/// Launch angle (radians above horizontal) to lob a shell from `shooter` to `target` under flat gravity `g`,
/// solved by bisection. Returns a finite angle; when the target is out of reach it returns the 45° maximum-range
/// loft (the round falls short, as a real mortar does).
pub fn ballistic_angle(shooter: [f32; 3], target: [f32; 3], speed: f32, g: f32) -> f32 {
    let dx = target[0] - shooter[0];
    let dz = target[2] - shooter[2];
    let d = crate::math::sqrt(dx * dx + dz * dz);
    let dh = target[1] - shooter[1];
    let v = det_max(speed, 1.0);
    let g = det_max(g, 1.0e-3);
    // Residual of the flat-earth range equation: dh = d tanθ - g d² / (2 v² cos²θ).
    let residual = |theta: f32| -> f32 {
        let (s, c) = (crate::math::sin(theta), crate::math::cos(theta));
        d * (s / c) - (g * d * d) / (2.0 * v * v * c * c) - dh
    };
    // If the target is unreachable, the residual stays negative everywhere; fall back to the 45° loft. Otherwise
    // bisect the low (shallow) arc.
    if residual(core::f32::consts::FRAC_PI_4) < 0.0 && residual(0.0) < 0.0 {
        return core::f32::consts::FRAC_PI_4;
    }
    let mut lo = 0.0f32;
    let mut hi = core::f32::consts::FRAC_PI_2 - 0.01;
    for _ in 0..32 {
        let mid = 0.5 * (lo + hi);
        if residual(mid) > 0.0 {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    0.5 * (lo + hi)
}

/// Aim direction for a mortar: the lobbed arc, returned as a unit vector the caller scales by the projectile
/// speed. Ballistic, no straight lead.
pub fn ballistic_aim(shooter: [f32; 3], target: [f32; 3], speed: f32, g: f32) -> [f32; 3] {
    let theta = ballistic_angle(shooter, target, speed, g);
    let dx = target[0] - shooter[0];
    let dz = target[2] - shooter[2];
    let d = det_max(crate::math::sqrt(dx * dx + dz * dz), 1.0e-3);
    let (s, c) = (crate::math::sin(theta), crate::math::cos(theta));
    [dx / d * c, s, dz / d * c]
}

/// Seeker lock rules: a seeker can lock a **unit** (never a structure/turret) whose range is inside the seeker's
/// band. Targets the team cannot perceive are already absent from the sighting board, so a "known enemy" is
/// lockable by construction.
pub fn seeker_can_lock(target_is_unit: bool, range: f32, band: [f32; 2]) -> bool {
    target_is_unit && range >= band[0] && range <= band[1]
}

/// Pick the hardpoint to fire at `range` against a target with the default mortar range
/// ([`MORTAR_MIN_RANGE_M`]); see [`choose_slot_with`].
pub fn choose_slot(ammo: &AmmoView, range: f32, target_is_unit: bool) -> Option<usize> {
    choose_slot_with(ammo, range, target_is_unit, MORTAR_MIN_RANGE_M)
}

/// Pick the hardpoint to fire at `range`: a mortar beyond `mortar_min_range_m` (structures only), a direct slot
/// inside its band, and a seeker only against a unit inside its band (never vs a structure). Returns `None` when
/// no usable slot fits the range.
pub fn choose_slot_with(
    ammo: &AmmoView,
    range: f32,
    target_is_unit: bool,
    mortar_min_range_m: f32,
) -> Option<usize> {
    let slots = &ammo.slots;
    // Collect usable slots with their role and band.
    let usable: Vec<(usize, &AmmoSlotView)> = slots
        .iter()
        .enumerate()
        .filter_map(|(i, s)| s.as_ref().filter(|s| slot_usable(s)).map(|s| (i, s)))
        .collect();
    if usable.is_empty() {
        return None;
    }
    let in_band = |s: &AmmoSlotView| range >= s.band[0] && range <= s.band[1];
    // 1. Mortar beyond the mortar minimum range, only against a structure: mortars lob un-led and cannot hit
    //    moving hulls well, so the AI reserves them for structures and fights hulls with direct fire (which leads
    //    the target).
    if range > mortar_min_range_m
        && !target_is_unit
        && let Some((i, _)) = usable
            .iter()
            .find(|(_, s)| s.role == WeaponRole::Mortar && in_band(s))
    {
        return Some(*i);
    }
    // 2. A direct slot whose band contains the range.
    if let Some((i, _)) = usable
        .iter()
        .find(|(_, s)| s.role == WeaponRole::Direct && in_band(s))
    {
        return Some(*i);
    }
    // 3. A seeker, only against a unit, inside its band.
    if target_is_unit
        && let Some((i, _)) = usable
            .iter()
            .find(|(_, s)| s.role == WeaponRole::Seeker && in_band(s))
    {
        return Some(*i);
    }
    // 4. Nothing fits: don't fire.
    None
}

/// Human-like aim error for a shot at a moving target. `lead_error_scale` magnifies the lead-point miss as a share
/// of the ideal lead distance (the target's travel during the shell's flight), `tracking_lag_ticks` is the
/// reaction delay before the reticle settles, and `spread_mrad` is a cone scaled by a random 0.5..1.5 drawn from
/// `rng`. Hard misses less; nothing about damage changes.
pub fn aim_error(aim: &AimParams, rng: &mut AiRng, lead_distance: f32) -> AimError {
    let lead = aim.lead_error_scale * lead_distance;
    let jitter = 0.5 + rng.next_f32(); // 0.5..1.5 deterministic spread multiplier
    AimError {
        lead,
        tracking_lag_ticks: aim.tracking_lag_ticks,
        spread_mrad: aim.spread_mrad * jitter,
    }
}

/// The hard deterministic fire gate. Returns `None` (hold fire) when: the unit is not weapons-free or holds fire,
/// no usable slot fits the target's range and type ([`choose_slot_with`]), there is no line of sight, or a
/// friendly sits in the shot cone ([`line_of_fire`]). Otherwise a [`FireIntent`] with the human-like aim error
/// ([`aim_error`], one draw from `rng`).
pub fn fire_gate<L: LineOfSight>(
    unit: &ShooterView,
    target: &FireTarget,
    aim: &AimParams,
    rng: &mut AiRng,
    los: L,
    friendlies: &[[f32; 3]],
) -> Option<FireIntent> {
    if !unit.weapons_free || unit.stance == Stance::HoldFire {
        return None;
    }
    let range = target.range;
    let slot = choose_slot_with(&unit.ammo, range, target.is_unit, unit.mortar_min_range_m)?;
    // Line of sight (no wall-hacks; a direct aimed shot still lands, but the AI never fires blind).
    if !los_clear(los, unit.pos, target.pos) {
        return None;
    }
    // Friendly-fire: hold when a friendly body is in the shot cone.
    if line_of_fire(unit.pos, target.pos, friendlies) {
        return None;
    }
    let speed = unit.ammo.slots[slot].as_ref()?.projectile_speed;
    let target_vel = target.vel;
    let tv = crate::math::sqrt(target_vel[0] * target_vel[0] + target_vel[2] * target_vel[2]);
    let tof = range / det_max(speed, 1.0);
    let lead_distance = tv * tof;
    let aim_error = aim_error(aim, rng, lead_distance);
    Some(FireIntent {
        slot,
        target: target.id,
        aim_error,
    })
}

/// The nearest hostile sighting of `team`'s board whose XZ distance from `pos` lies within `band`, or `None` (the
/// fire gate's callers use it to pick which enemy to shoot). Iterates the board in its canonical (entity-id)
/// order; of equal distances the first wins.
pub fn nearest_in_band(
    board: &SightingsBoard,
    team: u8,
    pos: [f32; 3],
    band: [f32; 2],
) -> Option<&KnownEnemy> {
    board
        .for_team(team)
        .iter()
        .filter(|k| {
            let dx = k.sighting.last_pos[0] - pos[0];
            let dz = k.sighting.last_pos[2] - pos[2];
            let d = crate::math::sqrt(dx * dx + dz * dz);
            d >= band[0] && d <= band[1]
        })
        .min_by(|a, b| {
            let da = dist_xz(a.sighting.last_pos, pos);
            let db = dist_xz(b.sighting.last_pos, pos);
            da.partial_cmp(&db).unwrap_or(core::cmp::Ordering::Equal)
        })
}

#[inline]
fn dist_xz(a: [f32; 3], b: [f32; 3]) -> f32 {
    let dx = a[0] - b[0];
    let dz = a[2] - b[2];
    crate::math::sqrt(dx * dx + dz * dz)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::OpenField;

    /// A cannon: direct, 120 m/s, 0-150 m (DO27's default band and its cannon's speed).
    fn cannon(ammo: Option<(u32, u32)>) -> Option<AmmoSlotView> {
        Some(AmmoSlotView {
            role: WeaponRole::Direct,
            projectile_speed: 120.0,
            ammo,
            band: [0.0, 150.0],
        })
    }

    /// A seeker: 200 m/s, 30-200 m.
    fn seeker() -> Option<AmmoSlotView> {
        Some(AmmoSlotView {
            role: WeaponRole::Seeker,
            projectile_speed: 200.0,
            ammo: None,
            band: [30.0, 200.0],
        })
    }

    /// A mortar: 90 m/s, 90-300 m.
    fn mortar() -> Option<AmmoSlotView> {
        Some(AmmoSlotView {
            role: WeaponRole::Mortar,
            projectile_speed: 90.0,
            ammo: None,
            band: [90.0, 300.0],
        })
    }

    fn unit() -> ShooterView {
        ShooterView::new(
            [0.0, 0.0, 0.0],
            AmmoView {
                slots: vec![cannon(None), seeker(), mortar()],
            },
        )
    }

    fn aim_params(lead: f32, lag: u32, spread: f32) -> AimParams {
        AimParams {
            lead_error_scale: lead,
            tracking_lag_ticks: lag,
            spread_mrad: spread,
        }
    }

    fn at(z: f32) -> FireTarget {
        FireTarget {
            id: 7,
            pos: [0.0, 0.0, z],
            vel: [0.0; 3],
            is_unit: true,
            range: z,
        }
    }

    #[test]
    fn lead_point_solves_the_intercept() {
        // Target 100 m ahead crossing at 10 m/s; a 120 m/s shell lands on it.
        let target = [0.0, 0.0, 100.0];
        let vel = [10.0, 0.0, 0.0];
        let aim = lead_point([0.0, 0.0, 0.0], target, vel, 120.0);
        // The aim point is ahead of the target by roughly vel * (100/120).
        assert!(
            aim[0] > 5.0,
            "lead aim point is ahead of the crossing target, x={}",
            aim[0]
        );
        assert!((aim[2] - 100.0).abs() < 1.0);
    }

    #[test]
    fn aim_error_scales_with_difficulty() {
        let easy = aim_params(1.0, 20, 35.0);
        let hard = aim_params(0.2, 3, 8.0);
        let mut rng = AiRng::new(1);
        let e = aim_error(&easy, &mut rng, 6.0);
        let mut rng = AiRng::new(1);
        let h = aim_error(&hard, &mut rng, 6.0);
        assert!(e.lead > h.lead, "Easy misses lead more");
        assert!(e.spread_mrad > h.spread_mrad, "Easy spread is wider");
        assert!(e.tracking_lag_ticks > h.tracking_lag_ticks);
    }

    #[test]
    fn weapon_choice_mortar_vs_structures_direct_vs_units() {
        let u = unit();
        // Far (150 m) vs a structure: the mortar (slot 2) — it tears down buildings and never has to lead a
        // moving hull.
        assert_eq!(choose_slot(&u.ammo, 150.0, false), Some(2));
        // Far (150 m) vs a unit: never the mortar; the direct cannon (slot 0) leads the target instead of
        // lobbing an un-led shell.
        assert_eq!(choose_slot(&u.ammo, 150.0, true), Some(0));
        // Close (40 m): direct (slot 0).
        assert_eq!(choose_slot(&u.ammo, 40.0, true), Some(0));
        // A structure at seeker range: never the seeker.
        let struct_band = choose_slot(&u.ammo, 100.0, false);
        assert_ne!(struct_band, Some(1), "seeker is never chosen vs a structure");
        // A dry direct slot at 40 m: the mortar is out of band (90 m+), so nothing fires — a dry slot is never
        // silently reused.
        let dry = AmmoView {
            slots: vec![cannon(Some((0, 1200))), mortar()],
        };
        assert_eq!(
            choose_slot(&dry, 40.0, true),
            None,
            "dry direct + mortar out of band → no fire"
        );
        // The same dry loadout at 120 m vs a unit: the direct slot is dry and the mortar is structures-only, so
        // nothing fires (no silent mortar).
        assert_eq!(
            choose_slot(&dry, 120.0, true),
            None,
            "mortar is never used on a hull"
        );
        // At 120 m vs a structure the mortar still fires (in band).
        assert_eq!(
            choose_slot(&dry, 120.0, false),
            Some(1),
            "mortar in band vs a structure"
        );
    }

    #[test]
    fn fire_gate_holds_without_los_and_for_dry_slots() {
        let u = unit();
        let mut rng = AiRng::new(2);
        // No terrain = open ground, LOS clear.
        let none = fire_gate(&u, &at(50.0), &aim_params(0.5, 8, 18.0), &mut rng, OpenField, &[]);
        assert!(none.is_some());
        // A friendly in the cone holds fire.
        let mut rng = AiRng::new(2);
        let blocked = fire_gate(
            &u,
            &at(50.0),
            &aim_params(0.5, 8, 18.0),
            &mut rng,
            OpenField,
            &[[0.0, 0.0, 20.0]],
        );
        assert!(blocked.is_none(), "friendly in the shot cone holds fire");
        // Dry slot + no fit: no fire.
        let mut dry = unit();
        dry.ammo = AmmoView {
            slots: vec![cannon(Some((0, 1200)))],
        }; // only a dry direct slot
        let mut rng = AiRng::new(2);
        let out = fire_gate(
            &dry,
            &at(50.0),
            &aim_params(0.5, 8, 18.0),
            &mut rng,
            OpenField,
            &[],
        );
        assert!(out.is_none(), "dry slot → no fire");
    }

    /// Not in DO27: the gate's other holds (no line of sight, hold fire, not weapons-free) and the mortar range as
    /// a setting.
    #[test]
    fn fire_gate_holds_behind_a_wall_and_on_orders() {
        let wall = crate::ai::MouthWall::new(25.0, [5.0, 10.0], [0.0, 10.0]);
        let mut rng = AiRng::new(2);
        let aim = aim_params(0.5, 8, 18.0);
        assert!(
            fire_gate(&unit(), &at(50.0), &aim, &mut rng, wall, &[]).is_none(),
            "no shot through rock"
        );
        assert!(
            fire_gate(
                &unit(),
                &at(50.0),
                &aim,
                &mut rng,
                None::<&crate::ai::MouthWall>,
                &[]
            )
            .is_some()
        );
        let mut holding = unit();
        holding.stance = Stance::HoldFire;
        assert!(fire_gate(&holding, &at(50.0), &aim, &mut rng, OpenField, &[]).is_none());
        let mut ordered_off = unit();
        ordered_off.weapons_free = false;
        assert!(fire_gate(&ordered_off, &at(50.0), &aim, &mut rng, OpenField, &[]).is_none());
        // A structure at 100 m: the mortar beyond the default 90 m, the cannon when the setting is 120 m.
        let structure = FireTarget {
            is_unit: false,
            ..at(100.0)
        };
        let slot = |u: &ShooterView, rng: &mut AiRng| {
            fire_gate(u, &structure, &aim, rng, OpenField, &[]).map(|i| i.slot)
        };
        assert_eq!(slot(&unit(), &mut rng), Some(2));
        let mut close_mortar = unit();
        close_mortar.mortar_min_range_m = 120.0;
        assert_eq!(slot(&close_mortar, &mut rng), Some(0));
    }

    #[test]
    fn ballistic_arc_reaches_the_target() {
        // 100 m flat shot with a 90 m/s shell: simulate the arc and check the range at the shooter's height is
        // within a few metres of the target.
        let shooter = [0.0, 0.0, 0.0];
        let target = [0.0, 0.0, 100.0];
        let dir = ballistic_aim(shooter, target, 90.0, 9.8);
        let v = [dir[0] * 90.0, dir[1] * 90.0, dir[2] * 90.0];
        let mut pos = [0.0, 0.0, 0.0];
        let mut vel = v;
        let dt = 1.0 / 60.0;
        let mut landed = f32::NAN;
        for _ in 0..10_000 {
            pos[0] += vel[0] * dt;
            pos[1] += vel[1] * dt;
            pos[2] += vel[2] * dt;
            vel[1] -= 9.8 * dt;
            if pos[1] <= 0.0 {
                landed = pos[2];
                break;
            }
        }
        assert!(
            (landed - 100.0).abs() < 6.0,
            "mortar lands near the target, range={landed}"
        );
    }

    #[test]
    fn seeker_lock_rejects_structures_and_out_of_band() {
        assert!(seeker_can_lock(true, 100.0, [30.0, 200.0]));
        assert!(
            !seeker_can_lock(false, 100.0, [30.0, 200.0]),
            "no lock on structures"
        );
        assert!(!seeker_can_lock(true, 20.0, [30.0, 200.0]), "too close to lock");
        assert!(!seeker_can_lock(true, 300.0, [30.0, 200.0]), "too far to lock");
    }

    /// Not in DO27: an unreachable target gets the 45° loft, and the nearest in-band sighting is picked from the
    /// board in entity-id order.
    #[test]
    fn out_of_reach_lofts_and_nearest_in_band_picks_the_closest() {
        let far = ballistic_angle([0.0; 3], [0.0, 0.0, 5000.0], 90.0, 9.8);
        assert_eq!(far, core::f32::consts::FRAC_PI_4);

        use crate::ai::{PerceptEnemy, PerceptUnit, PerceptionParams};
        let mut board = SightingsBoard::new(3, 1000.0);
        let observers = [PerceptUnit {
            id: 1,
            team: 1,
            pos: [0.0, 1.0, 0.0],
            sight_range_m: 1000.0,
            radar_scale: 1.0,
        }];
        let enemy = |id: u32, z: f32| PerceptEnemy {
            id,
            team: 2,
            pos: [0.0, 1.0, z],
            vel: [0.0, 0.0],
            dps: 10.0,
            band: [0.0, 100.0],
            lit: false,
        };
        let enemies = [enemy(20, 140.0), enemy(21, 60.0), enemy(22, 20.0)];
        board.refresh(
            OpenField,
            0,
            &observers,
            &enemies,
            &[PerceptionParams::default(); 3],
        );
        let pick = |band: [f32; 2]| nearest_in_band(&board, 1, [0.0; 3], band).map(|k| k.entity);
        assert_eq!(pick([0.0, 200.0]), Some(22));
        assert_eq!(pick([30.0, 200.0]), Some(21));
        assert_eq!(pick([150.0, 200.0]), None);
    }
}
