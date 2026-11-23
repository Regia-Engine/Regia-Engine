// SPDX-License-Identifier: MPL-2.0
//! Original headless AI-vs-AI sample for the S-18 harness (ADR-8151).
//!
//! Two teams pursue the nearest living opponent, breaking equal-distance ties by
//! stable unit index. Decisions read the beginning-of-tick world; movement and
//! damage apply simultaneously afterward. This small sample has no game assets,
//! ECS, renderer or network, and does not implement a game's combat or AI rules.

use crate::float::det_min;
use serde::{Deserialize, Serialize};

use crate::golden::{DetRun, DetScenario, StateHasher, TickRate, scenarios::SplitMix64};
use crate::harness::HarnessError;
use crate::hash::{Mix, SystemFold, Systems};
use crate::math;
use crate::replay::{DetInputSim, TickInputs};
use crate::state::{DetReader, DetState, DetStateError, DetWriter};

/// One team's order for one tick, the arena's input when a log or a seat drives it (DC-3.7b).
/// Seat 0 commands team 0 and seat 1 team 1; other seats are ignored. A team with no order this
/// tick (`None`, or not listed) behaves exactly as the seed-only scenario does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ArenaOrder {
    /// The team's units stay where they are this tick (they still attack a target in range).
    pub hold: bool,
    /// The team's units move at one and a half times the pursuit speed this tick.
    pub sprint: bool,
}

/// Host-configurable rules for the two-team sample.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ArenaSettings {
    /// Units on each of two teams; must be nonzero.
    pub units_per_team: u16,
    /// Initial distance of each team from the center in meters.
    pub spawn_radius: f32,
    /// Pursuit speed in meters per second.
    pub speed: f32,
    /// Attack range in meters; positive.
    pub attack_range: f32,
    /// Initial integer health of each unit; positive.
    pub health: u32,
    /// Damage per attack; positive.
    pub damage: u32,
    /// Minimum tick interval between attacks; positive.
    pub cooldown_ticks: u32,
}

impl Default for ArenaSettings {
    fn default() -> Self {
        Self {
            units_per_team: 4,
            spawn_radius: 24.0,
            speed: 6.0,
            attack_range: 3.0,
            health: 100,
            damage: 9,
            cooldown_ticks: 12,
        }
    }
}

impl ArenaSettings {
    /// Validate the rules. Distance/speed values must be finite and at most
    /// 1,000,000 to keep squared distances finite; speed may be zero.
    pub fn validate(self) -> Result<(), HarnessError> {
        let positive = [self.spawn_radius, self.attack_range]
            .into_iter()
            .all(|v| v.is_finite() && v > 0.0 && v <= 1_000_000.0);
        if !positive
            || !self.speed.is_finite()
            || !(0.0..=1_000_000.0).contains(&self.speed)
            || self.units_per_team == 0
            || self.health == 0
            || self.damage == 0
            || self.cooldown_ticks == 0
        {
            return Err(HarnessError::InvalidArenaSettings);
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct Unit {
    team: u32,
    position: [f32; 2],
    health: u32,
    cooldown: u32,
}

/// Observable counters; neither clock readings nor rendering enter them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArenaStats {
    /// Living units per team, in team 0, team 1 order.
    pub alive: [u32; 2],
    /// Remaining health per team, including zero-health units.
    pub health: [u64; 2],
    /// Attacks made by both teams; each attack hits its selected target.
    pub attacks: u64,
}

/// Reusable F-02 scenario. Reset reconstructs every authoritative state field.
#[derive(Debug, Clone)]
pub struct ArenaScenario {
    settings: ArenaSettings,
    units: Vec<Unit>,
    movement: Vec<[f32; 2]>,
    damage: Vec<u64>,
    fired: Vec<bool>,
    seed: u64,
    tick: u32,
    dt: f32,
    attacks: u64,
}

impl ArenaScenario {
    /// Validate settings and create an unbootstrapped scenario. A harness or
    /// F-02 recorder calls reset before stepping it.
    pub fn new(settings: ArenaSettings) -> Result<Self, HarnessError> {
        settings.validate()?;
        Ok(Self {
            settings,
            units: Vec::new(),
            movement: Vec::new(),
            damage: Vec::new(),
            fired: Vec::new(),
            seed: 0,
            tick: 0,
            dt: TickRate::DEFAULT.dt(),
            attacks: 0,
        })
    }

    /// Current combat counters.
    pub fn stats(&self) -> ArenaStats {
        let mut stats = ArenaStats {
            alive: [0; 2],
            health: [0; 2],
            attacks: self.attacks,
        };
        for unit in &self.units {
            let team = unit.team as usize;
            stats.alive[team] += u32::from(unit.health > 0);
            stats.health[team] += u64::from(unit.health);
        }
        stats
    }
}

impl DetScenario for ArenaScenario {
    fn name(&self) -> &str {
        "ai_arena"
    }

    fn default_run(&self) -> DetRun {
        DetRun::new(100, 1200, TickRate::DEFAULT)
    }

    fn reset(&mut self, run: &DetRun) {
        let _span = tracing::debug_span!("arena_bootstrap", seed = run.seed).entered();
        self.seed = run.seed;
        self.tick = 0;
        self.dt = run.rate.dt();
        self.attacks = 0;
        self.units.clear();
        let mut rng = SplitMix64::new(run.seed);
        for team in 0..2 {
            for _ in 0..self.settings.units_per_team {
                let side = if team == 0 { -1.0 } else { 1.0 };
                self.units.push(Unit {
                    team,
                    position: [
                        side * self.settings.spawn_radius,
                        rng.signed() * self.settings.spawn_radius,
                    ],
                    health: self.settings.health,
                    cooldown: 0,
                });
            }
        }
        self.movement.resize(self.units.len(), [0.0; 2]);
        self.damage.resize(self.units.len(), 0);
        self.fired.resize(self.units.len(), false);
    }

    fn step(&mut self, tick: u32) {
        self.step_orders(tick, [ArenaOrder::default(); 2]);
    }

    fn hash_state(&self, h: &mut StateHasher) {
        h.u64("seed", self.seed);
        h.u32("tick", self.tick);
        h.f32("dt", self.dt);
        h.u32("units_per_team", u32::from(self.settings.units_per_team));
        h.f32("spawn_radius", self.settings.spawn_radius);
        h.f32("speed", self.settings.speed);
        h.f32("attack_range", self.settings.attack_range);
        h.u32("initial_health", self.settings.health);
        h.u32("damage", self.settings.damage);
        h.u32("cooldown_ticks", self.settings.cooldown_ticks);
        h.u64("attacks", self.attacks);
        h.u64("units", self.units.len() as u64);
        for (index, unit) in self.units.iter().enumerate() {
            h.item("unit", index as u32);
            h.u32("team", unit.team);
            h.f32("x", unit.position[0]);
            h.f32("y", unit.position[1]);
            h.u32("health", unit.health);
            h.u32("cooldown", unit.cooldown);
        }
    }
}

impl ArenaScenario {
    /// The one arena step: the seed-only [`DetScenario::step`] and the input-driven
    /// [`DetInputSim::step`] both land here (default orders are the seed-only behavior, bit for bit).
    fn step_orders(&mut self, tick: u32, orders: [ArenaOrder; 2]) {
        let _span = tracing::trace_span!("arena_step", tick).entered();
        self.damage.fill(0);
        self.fired.fill(false);
        self.movement.fill([0.0; 2]);
        let range_squared = self.settings.attack_range * self.settings.attack_range;
        for (index, unit) in self.units.iter().enumerate() {
            if unit.health == 0 {
                continue;
            }
            let mut target = None;
            let mut nearest = f32::INFINITY;
            for (other_index, other) in self.units.iter().enumerate() {
                if other.team == unit.team || other.health == 0 {
                    continue;
                }
                let dx = other.position[0] - unit.position[0];
                let dy = other.position[1] - unit.position[1];
                let distance_squared = dx * dx + dy * dy;
                if distance_squared < nearest {
                    nearest = distance_squared;
                    target = Some(other_index);
                }
            }
            let Some(target) = target else { continue };
            if nearest <= range_squared {
                if unit.cooldown <= 1 {
                    self.damage[target] += u64::from(self.settings.damage);
                    self.attacks += 1;
                    self.fired[index] = true;
                }
            } else if !orders[unit.team as usize & 1].hold {
                let speed = if orders[unit.team as usize & 1].sprint {
                    self.settings.speed * 1.5
                } else {
                    self.settings.speed
                };
                let distance = math::sqrt(nearest);
                let travel = det_min(speed * self.dt, distance - self.settings.attack_range);
                for axis in 0..2 {
                    self.movement[index][axis] =
                        (self.units[target].position[axis] - unit.position[axis]) / distance * travel;
                }
            }
        }
        for (index, unit) in self.units.iter_mut().enumerate() {
            unit.cooldown = if self.fired[index] {
                self.settings.cooldown_ticks
            } else {
                unit.cooldown.saturating_sub(1)
            };
            unit.position[0] += self.movement[index][0];
            unit.position[1] += self.movement[index][1];
            unit.health = unit
                .health
                .saturating_sub(self.damage[index].min(u64::from(u32::MAX)) as u32);
        }
        self.tick = tick.saturating_add(1);
    }
}

/// The arena's desync systems: `units` (positions and the movement scratch), `combat` (health,
/// cooldowns, the attack count and the damage scratch) and `rules` (settings, seed, tick, `dt`, teams).
impl SystemFold<3> for ArenaScenario {
    const SYSTEMS: Systems<3> = Systems::new(["units", "combat", "rules"]);

    fn fold_systems(&self, mix: &mut [Mix; 3]) {
        let [units, combat, rules] = mix;
        units.u64(self.units.len() as u64);
        combat.u64(self.attacks);
        let s = &self.settings;
        rules.u64(u64::from(s.units_per_team));
        for v in [s.spawn_radius, s.speed, s.attack_range, self.dt] {
            rules.f32(v);
        }
        for v in [s.health, s.damage, s.cooldown_ticks, self.tick] {
            rules.u64(u64::from(v));
        }
        rules.u64(self.seed);
        for (index, unit) in self.units.iter().enumerate() {
            units.f32(unit.position[0]);
            units.f32(unit.position[1]);
            rules.u64(u64::from(unit.team));
            combat.u64(u64::from(unit.health));
            combat.u64(u64::from(unit.cooldown));
            if let Some(m) = self.movement.get(index) {
                units.f32(m[0]);
                units.f32(m[1]);
            }
            if let (Some(d), Some(f)) = (self.damage.get(index), self.fired.get(index)) {
                combat.u64(*d);
                combat.u64(u64::from(*f));
            }
        }
    }

    fn inject(&mut self, system: usize) -> bool {
        match system {
            0 => match self.units.first_mut() {
                Some(unit) => unit.position[0] = f32::from_bits(unit.position[0].to_bits() ^ 1),
                None => return false,
            },
            1 => self.attacks = self.attacks.wrapping_add(1),
            2 => self.seed ^= 1,
            _ => return false,
        }
        true
    }
}

/// Input-driven arena (DC-3.7b): seat 0 orders team 0, seat 1 team 1 ([`ArenaOrder`]). Steps through
/// the same code as [`DetScenario::step`]; ticks past `u32::MAX` saturate.
impl DetInputSim<3> for ArenaScenario {
    type Input = ArenaOrder;

    fn step(&mut self, tick: u64, inputs: &TickInputs<ArenaOrder>) {
        let orders = [
            inputs.get(0).copied().unwrap_or_default(),
            inputs.get(1).copied().unwrap_or_default(),
        ];
        self.step_orders(u32::try_from(tick).unwrap_or(u32::MAX), orders);
    }
}

/// Saves the settings, every unit, the per-tick scratch (so a restore is bit-identical even between
/// steps), the seed, tick, `dt` and attack count (DC-3.7a). `load` refuses settings that fail
/// [`ArenaSettings::validate`], a team other than 0 or 1, and scratch whose length is not the unit
/// count (`bad_value`): `step` indexes by them.
impl DetState for ArenaScenario {
    const STATE_TAG: [u8; 8] = *b"aiarena\0";
    const STATE_VERSION: u32 = 1;

    fn save(&self, w: &mut DetWriter) {
        let s = &self.settings;
        w.u16(s.units_per_team);
        w.f32(s.spawn_radius);
        w.f32(s.speed);
        w.f32(s.attack_range);
        w.u32(s.health);
        w.u32(s.damage);
        w.u32(s.cooldown_ticks);
        w.seq(&self.units, |w, u| {
            w.u32(u.team);
            w.f32(u.position[0]);
            w.f32(u.position[1]);
            w.u32(u.health);
            w.u32(u.cooldown);
        });
        w.seq(&self.movement, |w, m| {
            w.f32(m[0]);
            w.f32(m[1]);
        });
        w.seq(&self.damage, |w, d| w.u64(*d));
        w.seq(&self.fired, |w, f| w.bool(*f));
        w.u64(self.seed);
        w.u32(self.tick);
        w.f32(self.dt);
        w.u64(self.attacks);
    }

    fn load(r: &mut DetReader<'_>) -> Result<Self, DetStateError> {
        let settings = ArenaSettings {
            units_per_team: r.u16()?,
            spawn_radius: r.f32()?,
            speed: r.f32()?,
            attack_range: r.f32()?,
            health: r.u32()?,
            damage: r.u32()?,
            cooldown_ticks: r.u32()?,
        };
        settings
            .validate()
            .map_err(|e| DetStateError::bad_value(format_args!("arena settings: {e}")))?;
        let units = r.seq(|r| {
            let team = r.u32()?;
            if team > 1 {
                return Err(DetStateError::bad_value(format_args!("arena team {team}")));
            }
            Ok(Unit {
                team,
                position: [r.f32()?, r.f32()?],
                health: r.u32()?,
                cooldown: r.u32()?,
            })
        })?;
        let movement = r.seq(|r| Ok([r.f32()?, r.f32()?]))?;
        let damage = r.seq(|r| r.u64())?;
        let fired = r.seq(|r| r.bool())?;
        let n = units.len();
        if movement.len() != n || damage.len() != n || fired.len() != n {
            return Err(DetStateError::bad_value(format_args!(
                "arena scratch of {}, {} and {} entries for {n} units",
                movement.len(),
                damage.len(),
                fired.len()
            )));
        }
        Ok(ArenaScenario {
            settings,
            units,
            movement,
            damage,
            fired,
            seed: r.u64()?,
            tick: r.u32()?,
            dt: r.f32()?,
            attacks: r.u64()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_distance_targets_use_stable_index_and_skip_dead_units() {
        for first_target_alive in [true, false] {
            let mut arena = ArenaScenario::new(ArenaSettings {
                units_per_team: 2,
                ..ArenaSettings::default()
            })
            .expect("settings");
            arena.reset(&arena.default_run());
            for unit in &mut arena.units {
                unit.position = [0.0; 2];
                unit.cooldown = 100;
            }
            arena.units[0].cooldown = 0;
            arena.units[1].health = 0;
            if !first_target_alive {
                arena.units[2].health = 0;
            }
            DetScenario::step(&mut arena, 0);
            assert_eq!(arena.attacks, 1);
            let target = if first_target_alive { 2 } else { 3 };
            assert_eq!(arena.units[target].health, 91);
            if first_target_alive {
                assert_eq!(arena.units[3].health, 100);
            }
        }
    }

    #[test]
    fn simultaneous_damage_saturates_instead_of_overflowing_health() {
        let settings = ArenaSettings {
            units_per_team: 3,
            damage: u32::MAX,
            health: u32::MAX,
            ..ArenaSettings::default()
        };
        let mut arena = ArenaScenario::new(settings).expect("settings");
        arena.reset(&arena.default_run());
        for unit in &mut arena.units {
            unit.position = [0.0; 2];
        }
        DetScenario::step(&mut arena, 0);
        assert_eq!(arena.damage[0], 3 * u64::from(u32::MAX));
        assert_eq!(arena.damage[3], 3 * u64::from(u32::MAX));
        assert_eq!(arena.stats().attacks, 6);
        assert_eq!(arena.stats().alive, [2, 2]);
        assert_eq!(arena.stats().health, [2 * u64::from(u32::MAX); 2]);
    }
}
