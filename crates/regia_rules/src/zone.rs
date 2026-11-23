// SPDX-License-Identifier: MPL-2.0
//! A shrinking play zone (battle royale), stepped by the session once per match tick (GR-11.3).
//!
//! A [`ZoneSchedule`] is a start circle and a list of [`ZonePhase`]s. Each phase waits
//! `wait_ticks`, then shrinks the circle over `shrink_ticks` to `end_radius` around a new center.
//! The new center is drawn inside the current circle so that the next circle fits inside it, from
//! a [`DetRng`] stream of the schedule's seed and the match tick the zone started on: every peer
//! with the same settings draws the same circles. After the last phase the circle holds.
//!
//! The circle is kept in whole centimeters (`i64`) across XZ. The shrink is an integer linear
//! interpolation; [`ZoneState::inside`] is a squared-distance test in `i128`. No square root and
//! no trigonometry run. A live unit outside the current circle gets one [`RulesEvent::ZoneDamage`]
//! per match tick with the phase's `damage_per_tick`; the game applies it (the rules core never
//! owns health). Each stage change sends a [`RulesEvent::ZonePhase`].

use regia_detsim::rng::DetRng;
use serde::{Deserialize, Serialize};

use crate::RulesError;
use crate::objective::{Contestant, RulesEvent};

/// Stream id of the zone's generator ("ZONE").
pub const ZONE_RNG_STREAM: u64 = 0x5a4f_4e45;

/// Most phases a schedule may list.
pub const MAX_ZONE_PHASES: usize = 64;

/// Largest radius a schedule may use, in world units (1,000 km).
pub const MAX_ZONE_RADIUS: f32 = 1_000_000.0;

/// Draws per center pick before the pick keeps the current center.
const CENTER_TRIES: u32 = 32;

/// One phase of a [`ZoneSchedule`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ZonePhase {
    /// Match ticks the circle holds before this phase's shrink starts.
    pub wait_ticks: u64,
    /// Match ticks the shrink takes. `0` jumps to the new circle.
    pub shrink_ticks: u64,
    /// Radius at the end of the shrink, world units. At most the radius before the phase.
    pub end_radius: f32,
    /// Damage per match tick a live unit outside the circle takes during this phase (its wait and
    /// its shrink). The last phase's value holds after the schedule ends.
    pub damage_per_tick: u32,
}

/// A shrinking zone: the start circle, its phases and the seed of its center picks.
///
/// Off by default (`SessionSettings::zone` is `None`, S-14's behavior).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ZoneSchedule {
    /// Center of the start circle across XZ (`[x, z]`), world units.
    pub center: [f32; 2],
    /// Radius of the start circle, world units.
    pub radius: f32,
    /// Phases in play order.
    pub phases: Vec<ZonePhase>,
    /// Seed of the center picks. Every peer of a lockstep match uses the same value.
    pub seed: u64,
}

impl Default for ZoneSchedule {
    fn default() -> Self {
        Self {
            center: [0.0, 0.0],
            radius: 1000.0,
            phases: Vec::new(),
            seed: 0,
        }
    }
}

impl ZoneSchedule {
    /// Refuse a schedule a match cannot run: a radius that is not finite, negative or past
    /// [`MAX_ZONE_RADIUS`], an `end_radius` above the radius before its phase, or more than
    /// [`MAX_ZONE_PHASES`] phases. [`RulesError::ShrinkZoneSettings`] names the field.
    pub fn validate(&self) -> Result<(), RulesError> {
        let bad = |field: &'static str| Err(RulesError::ShrinkZoneSettings { field });
        // A center whose centimeter conversion saturates is refused like one that is not finite
        // (rules-genre-br-rts-review finding A): `to_cm` would pin it to the `i64` edge.
        if !(cm_fits(self.center[0]) && cm_fits(self.center[1])) {
            return bad("center");
        }
        if !radius_ok(self.radius) {
            return bad("radius");
        }
        if self.phases.len() > MAX_ZONE_PHASES {
            return bad("phases");
        }
        let mut before = self.radius;
        for phase in &self.phases {
            if !radius_ok(phase.end_radius) || phase.end_radius > before {
                return bad("end_radius");
            }
            before = phase.end_radius;
        }
        Ok(())
    }
}

fn radius_ok(radius: f32) -> bool {
    radius.is_finite() && (0.0..=MAX_ZONE_RADIUS).contains(&radius)
}

/// True when `meters` is finite and its centimeter value lies strictly inside the `i64` range
/// (2^63 centimeters either way), so [`to_cm`] does not saturate it.
fn cm_fits(meters: f32) -> bool {
    const EDGE_CM: f32 = 9_223_372_036_854_775_808.0; // 2^63, exact in f32
    let cm = meters * 100.0;
    cm.is_finite() && cm.abs() < EDGE_CM
}

/// `meters` in whole centimeters, toward zero. Callers check finiteness first.
fn to_cm(meters: f32) -> i64 {
    (meters * 100.0) as i64
}

/// `cm` in world units.
fn to_m(cm: i64) -> f32 {
    cm as f32 * 0.01
}

/// A circle across XZ in whole centimeters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ZoneCircle {
    /// Center `[x, z]`, centimeters.
    pub center_cm: [i64; 2],
    /// Radius, centimeters.
    pub radius_cm: i64,
}

impl ZoneCircle {
    /// The circle of `center` (`[x, z]`) and `radius` in world units. A value that is not finite
    /// reads as 0.
    pub fn from_world(center: [f32; 2], radius: f32) -> Self {
        let cm = |v: f32| if v.is_finite() { to_cm(v) } else { 0 };
        Self {
            center_cm: [cm(center[0]), cm(center[1])],
            radius_cm: cm(radius).max(0),
        }
    }

    /// True when the XZ position of `pos` is inside or on the circle. A position that is not
    /// finite is outside, and a negative radius reads as 0.
    ///
    /// An axis gap wider than the radius is outside before anything is squared, so every input
    /// stays inside `i128`: a far unit, a saturated center from a restored circle, or both
    /// (rules-genre-br-rts-review finding A). The squares that remain are each at most
    /// `(2^63 - 1)^2`, and two of them fit.
    pub fn contains(&self, pos: [f32; 3]) -> bool {
        if !(pos[0].is_finite() && pos[2].is_finite()) {
            return false;
        }
        let dx = i128::from(to_cm(pos[0])) - i128::from(self.center_cm[0]);
        let dz = i128::from(to_cm(pos[2])) - i128::from(self.center_cm[1]);
        let r = i128::from(self.radius_cm.max(0));
        if dx.abs() > r || dz.abs() > r {
            return false;
        }
        dx * dx + dz * dz <= r * r
    }

    /// Center `[x, z]` in world units.
    pub fn center(&self) -> [f32; 2] {
        [to_m(self.center_cm[0]), to_m(self.center_cm[1])]
    }

    /// Radius in world units.
    pub fn radius(&self) -> f32 {
        to_m(self.radius_cm)
    }
}

/// Where the zone is in its schedule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ZoneStage {
    /// The zone has not started (no match played yet, or no schedule).
    #[default]
    Idle,
    /// The circle holds before the phase's shrink.
    Waiting,
    /// The circle shrinks toward the phase's target.
    Shrinking,
    /// Every phase ran; the last circle holds.
    Closed,
}

impl ZoneStage {
    /// The stage as a lowercase word (`"waiting"`, ...).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Waiting => "waiting",
            Self::Shrinking => "shrinking",
            Self::Closed => "closed",
        }
    }
}

/// The running zone of a match. A match launch (and a round reset) puts it back to
/// [`ZoneStage::Idle`]; it starts on the first match tick the match plays.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ZoneState {
    /// The match tick the zone started on.
    pub started_at: Option<u64>,
    /// The phase now running, from 0. Equals the phase count once closed.
    pub phase: u32,
    /// The stage of that phase.
    pub stage: ZoneStage,
    /// The match tick the stage began on.
    pub stage_since: u64,
    /// The circle at the start of the phase (the circle during its wait).
    pub from: ZoneCircle,
    /// The circle at the end of the phase's shrink.
    pub to: ZoneCircle,
    /// The circle on the last stepped tick.
    pub current: ZoneCircle,
    /// The center-pick generator.
    pub rng: DetRng,
}

impl Default for ZoneState {
    fn default() -> Self {
        Self {
            started_at: None,
            phase: 0,
            stage: ZoneStage::Idle,
            stage_since: 0,
            from: ZoneCircle::default(),
            to: ZoneCircle::default(),
            current: ZoneCircle::default(),
            rng: DetRng::seed_from(0).stream(ZONE_RNG_STREAM),
        }
    }
}

impl ZoneState {
    /// Back to [`ZoneStage::Idle`] (a launch or a round reset).
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// True when `pos` is inside the current circle. An idle zone holds everything.
    pub fn inside(&self, pos: [f32; 3]) -> bool {
        self.stage == ZoneStage::Idle || self.current.contains(pos)
    }

    /// Run match tick `tick`: start the zone on its first tick, move through the stages that are
    /// due, set the current circle, then report [`RulesEvent::ZoneDamage`] for every live unit in
    /// `units` (host order) outside it. Stage changes push [`RulesEvent::ZonePhase`].
    pub fn tick(
        &mut self,
        schedule: &ZoneSchedule,
        tick: u64,
        units: &[Contestant],
        events: &mut Vec<RulesEvent>,
    ) {
        if self.started_at.is_none() {
            self.start(schedule, tick, events);
        }
        // Each pass moves one stage on; a schedule has at most two stages per phase.
        for _ in 0..=(2 * schedule.phases.len()) {
            if !self.advance(schedule, tick, events) {
                break;
            }
        }
        if self.stage == ZoneStage::Shrinking
            && let Some(phase) = schedule.phases.get(self.phase as usize)
        {
            let elapsed = tick.saturating_sub(self.stage_since);
            self.current = lerp(self.from, self.to, elapsed, phase.shrink_ticks);
        }
        let amount = self.damage(schedule);
        if amount == 0 {
            return;
        }
        for unit in units {
            if unit.alive && !self.current.contains(unit.position) {
                events.push(RulesEvent::ZoneDamage {
                    tick,
                    unit: unit.id,
                    amount,
                });
            }
        }
    }

    /// Damage per tick outside the circle now: the running phase's, or the last phase's once
    /// closed. `0` while idle or without phases.
    pub fn damage(&self, schedule: &ZoneSchedule) -> u32 {
        match self.stage {
            ZoneStage::Idle => 0,
            ZoneStage::Closed => schedule.phases.last().map_or(0, |p| p.damage_per_tick),
            _ => schedule
                .phases
                .get(self.phase as usize)
                .map_or(0, |p| p.damage_per_tick),
        }
    }

    fn start(&mut self, schedule: &ZoneSchedule, tick: u64, events: &mut Vec<RulesEvent>) {
        let circle = ZoneCircle::from_world(schedule.center, schedule.radius);
        // The start tick is agreed by every peer: a later match on a session clock draws other
        // circles from the same seed.
        let seed = schedule.seed ^ tick.wrapping_mul(0x9e37_79b9_7f4a_7c15);
        self.started_at = Some(tick);
        self.phase = 0;
        self.stage_since = tick;
        self.from = circle;
        self.current = circle;
        self.rng = DetRng::seed_from(seed).stream(ZONE_RNG_STREAM);
        match schedule.phases.first() {
            Some(phase) => {
                self.stage = ZoneStage::Waiting;
                self.to = pick_next(&mut self.rng, circle, phase.end_radius);
            }
            None => {
                self.stage = ZoneStage::Closed;
                self.to = circle;
            }
        }
        self.push_phase(tick, events);
    }

    /// Move one stage on when it is due at `tick`. Returns whether it moved.
    fn advance(&mut self, schedule: &ZoneSchedule, tick: u64, events: &mut Vec<RulesEvent>) -> bool {
        let Some(phase) = schedule.phases.get(self.phase as usize).copied() else {
            if self.stage != ZoneStage::Closed {
                self.stage = ZoneStage::Closed;
                self.push_phase(tick, events);
            }
            return false;
        };
        let elapsed = tick.saturating_sub(self.stage_since);
        match self.stage {
            ZoneStage::Waiting if elapsed >= phase.wait_ticks => {
                self.stage = ZoneStage::Shrinking;
                self.stage_since = self.stage_since.saturating_add(phase.wait_ticks);
                self.push_phase(tick, events);
                true
            }
            ZoneStage::Shrinking if elapsed >= phase.shrink_ticks => {
                self.current = self.to;
                self.stage_since = self.stage_since.saturating_add(phase.shrink_ticks);
                self.phase = self.phase.saturating_add(1);
                self.from = self.to;
                match schedule.phases.get(self.phase as usize) {
                    Some(next) => {
                        self.stage = ZoneStage::Waiting;
                        self.to = pick_next(&mut self.rng, self.from, next.end_radius);
                    }
                    None => self.stage = ZoneStage::Closed,
                }
                self.push_phase(tick, events);
                true
            }
            _ => false,
        }
    }

    fn push_phase(&self, tick: u64, events: &mut Vec<RulesEvent>) {
        events.push(RulesEvent::ZonePhase {
            tick,
            phase: self.phase,
            stage: self.stage,
            center: self.current.center(),
            radius: self.current.radius(),
            target_center: self.to.center(),
            target_radius: self.to.radius(),
        });
    }
}

/// The circle `elapsed` of `total` ticks from `from` to `to` (integer interpolation).
fn lerp(from: ZoneCircle, to: ZoneCircle, elapsed: u64, total: u64) -> ZoneCircle {
    if total == 0 || elapsed >= total {
        return to;
    }
    let step = |a: i64, b: i64| -> i64 {
        let delta = i128::from(b) - i128::from(a);
        // `|delta|` spans at most `2^64 - 1` centimeters and `elapsed` stays below `2^64`, so the
        // product always fits `u128`. Multiplying before the divide keeps the circle on the exact
        // integer interpolation for every restored circle, even at the `i64` and `u64` edges; a
        // divide-first fallback can land up to `total` centimeters short and keep a unit inside a
        // circle it should have left (rules-pve-br-rts-fixes verification, finding 1).
        let span = delta.unsigned_abs();
        let moved = span * u128::from(elapsed) / u128::from(total);
        // `moved` is at most `span` because `elapsed < total`, so `a ± moved` stays on the `a`..`b`
        // segment and every conversion below succeeds; the `min` and `unwrap_or` arms only keep a
        // hostile restore panic-free.
        let moved = moved.min(i128::MAX as u128) as i128;
        let shifted = if delta.is_negative() {
            i128::from(a) - moved
        } else {
            i128::from(a) + moved
        };
        i64::try_from(shifted).unwrap_or(b)
    };
    ZoneCircle {
        center_cm: [
            step(from.center_cm[0], to.center_cm[0]),
            step(from.center_cm[1], to.center_cm[1]),
        ],
        radius_cm: step(from.radius_cm, to.radius_cm),
    }
}

/// The next circle of radius `end_radius` inside `current`: its center is drawn uniformly from
/// the disc of radius `current - end` around the current center (rejection sampling of whole
/// centimeters in the bounding square, at most `CENTER_TRIES` draws; the last fallback keeps the
/// center).
pub fn pick_next(rng: &mut DetRng, current: ZoneCircle, end_radius: f32) -> ZoneCircle {
    let end = if end_radius.is_finite() {
        to_cm(end_radius).clamp(0, current.radius_cm)
    } else {
        current.radius_cm
    };
    let slack = current.radius_cm.saturating_sub(end);
    let mut center = current.center_cm;
    if slack > 0 {
        let span = u64::try_from(slack)
            .unwrap_or(0)
            .saturating_mul(2)
            .saturating_add(1);
        let reach = i128::from(slack) * i128::from(slack);
        for _ in 0..CENTER_TRIES {
            let dx = i64::try_from(rng.range_u64(span)).unwrap_or(0) - slack;
            let dz = i64::try_from(rng.range_u64(span)).unwrap_or(0) - slack;
            if i128::from(dx) * i128::from(dx) + i128::from(dz) * i128::from(dz) <= reach {
                center = [
                    current.center_cm[0].saturating_add(dx),
                    current.center_cm[1].saturating_add(dz),
                ];
                break;
            }
        }
    }
    ZoneCircle {
        center_cm: center,
        radius_cm: end,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schedule(seed: u64) -> ZoneSchedule {
        ZoneSchedule {
            center: [0.0, 0.0],
            radius: 500.0,
            phases: vec![
                ZonePhase {
                    wait_ticks: 10,
                    shrink_ticks: 20,
                    end_radius: 200.0,
                    damage_per_tick: 1,
                },
                ZonePhase {
                    wait_ticks: 5,
                    shrink_ticks: 10,
                    end_radius: 50.0,
                    damage_per_tick: 4,
                },
            ],
            seed,
        }
    }

    fn run(s: &ZoneSchedule, ticks: u64) -> (ZoneState, Vec<RulesEvent>) {
        let mut state = ZoneState::default();
        let mut events = Vec::new();
        for tick in 1..=ticks {
            state.tick(s, tick, &[], &mut events);
        }
        (state, events)
    }

    #[test]
    fn every_next_circle_fits_inside_the_one_before() {
        for seed in 0..200 {
            let s = schedule(seed);
            let mut rng = DetRng::seed_from(seed).stream(ZONE_RNG_STREAM);
            let mut circle = ZoneCircle::from_world(s.center, s.radius);
            for phase in &s.phases {
                let next = pick_next(&mut rng, circle, phase.end_radius);
                let dx = i128::from(next.center_cm[0] - circle.center_cm[0]);
                let dz = i128::from(next.center_cm[1] - circle.center_cm[1]);
                let slack = i128::from(circle.radius_cm - next.radius_cm);
                assert!(dx * dx + dz * dz <= slack * slack, "seed {seed}");
                circle = next;
            }
        }
    }

    #[test]
    fn stages_run_in_order_and_the_last_circle_holds() {
        let s = schedule(7);
        let (state, events) = run(&s, 100);
        let stages: Vec<(u64, u32, ZoneStage)> = events
            .iter()
            .filter_map(|e| match e {
                RulesEvent::ZonePhase {
                    tick, phase, stage, ..
                } => Some((*tick, *phase, *stage)),
                _ => None,
            })
            .collect();
        assert_eq!(
            stages,
            vec![
                (1, 0, ZoneStage::Waiting),
                (11, 0, ZoneStage::Shrinking),
                (31, 1, ZoneStage::Waiting),
                (36, 1, ZoneStage::Shrinking),
                (46, 2, ZoneStage::Closed),
            ]
        );
        assert_eq!(state.stage, ZoneStage::Closed);
        assert_eq!(state.current.radius_cm, 5_000);
        assert_eq!(state.damage(&s), 4, "the last phase's damage holds");
    }

    #[test]
    fn zero_length_stages_jump_on_one_tick() {
        let s = ZoneSchedule {
            radius: 100.0,
            phases: vec![ZonePhase {
                wait_ticks: 0,
                shrink_ticks: 0,
                end_radius: 10.0,
                damage_per_tick: 2,
            }],
            ..ZoneSchedule::default()
        };
        let (state, events) = run(&s, 1);
        assert_eq!(state.stage, ZoneStage::Closed);
        assert_eq!(state.current.radius_cm, 1_000);
        assert_eq!(events.len(), 3, "waiting, shrinking, closed on tick 1");
    }

    #[test]
    fn validate_names_the_bad_field() {
        assert!(schedule(1).validate().is_ok());
        let mut grow = schedule(1);
        grow.phases[1].end_radius = 300.0;
        assert_eq!(
            grow.validate(),
            Err(RulesError::ShrinkZoneSettings { field: "end_radius" })
        );
        let mut nan = schedule(1);
        nan.radius = f32::NAN;
        assert_eq!(
            nan.validate(),
            Err(RulesError::ShrinkZoneSettings { field: "radius" })
        );
        let mut many = schedule(1);
        many.phases = vec![many.phases[1]; MAX_ZONE_PHASES + 1];
        many.phases[0].end_radius = 50.0;
        assert_eq!(
            many.validate(),
            Err(RulesError::ShrinkZoneSettings { field: "phases" })
        );
    }

    #[test]
    fn a_non_finite_position_is_outside() {
        let circle = ZoneCircle::from_world([0.0, 0.0], 10.0);
        assert!(circle.contains([10.0, 99.0, 0.0]), "on the edge, any height");
        assert!(!circle.contains([10.01, 0.0, 0.0]));
        assert!(!circle.contains([f32::NAN, 0.0, 0.0]));
        assert!(!circle.contains([0.0, 0.0, f32::INFINITY]));
    }
}
