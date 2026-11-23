// SPDX-License-Identifier: MPL-2.0
//! Impact damage (ADR-3272): contacts on an opted-in structure's intact colliders become
//! destruction hits, so a car or a heavy crate that hits a wall breaks it.
//!
//! After each physics step, on an [`Authority::Server`] world only, [`DestructionImpact::convert`]
//! reads the narrow phase (read only) for every intact chunk collider of the structures enabled with
//! [`DestructionImpact::enable`]. Each contact pair gives one measurement: its impulse (newton-seconds:
//! rapier's `total_impulse_magnitude`, the contact-force event's force times the step), the contact
//! point (the impulse-weighted mean of the manifold points on the structure side), the direction
//! into the chunk and the contact speed (the impulse over the pair's reduced linear mass: the normal
//! speed the contact took out of the pair in this step). The chunk under the contact is the
//! collider's chunk. Then, in order:
//!
//! 1. the other collider is debris or another structure: skipped unless
//!    [`ImpactSettings::debris_hits`] (off by default: no cascades);
//! 2. impulse times [`ImpactSettings::impulse_scale`] times the material's `damage_scale` below the
//!    material's `min_impulse`: no damage (the break model would drop it too);
//! 3. contact speed below [`ImpactSettings::min_speed`]: a resting or pushing contact (its speed is
//!    about `g dt` or `F dt / m`);
//! 4. the pair hit less than [`ImpactSettings::cooldown_ticks`] ago: cooling down.
//!
//! The survivors are sorted strongest first (ties by instance, chunk and the other collider's
//! handle) and the first [`ImpactSettings::max_hits_per_tick`] are queued as
//! [`DamageKind::Impulse`] hits through [`DestructionWorld::damage`]: they apply at the next
//! destruction step with the material's `damage_scale` and `min_impulse`, the break cap and the
//! support graph, and a server's breaks reach its clients as ordinary break events. Pairs are read
//! in instance, chunk and handle order, never in map or graph order. The buffers are kept between
//! ticks: a quiet or resting tick allocates nothing.
//!
//! Guide: `book/agents/destruction.md` ("Impact damage").

use std::collections::BTreeSet;

use rapier3d::geometry::ColliderHandle;
use regia_app::prelude::*;
use regia_core::math::Vec3;
use serde::{Deserialize, Serialize};

use super::bridge::DestructionPhysics;
use super::plugin::Destruction;
use super::sim::{Authority, Damage, DamageKind, DestructibleId, DestructionWorld};
use crate::convert::{from_rapier_pose, from_rapier_vec3};
use crate::world::PhysicsWorld;

/// Largest [`ImpactSettings::max_hits_per_tick`] accepted.
pub const MAX_IMPACT_HITS_PER_TICK: u32 = 4096;
/// Largest [`ImpactSettings::cooldown_ticks`] accepted (10 minutes at 60 Hz).
pub const MAX_IMPACT_COOLDOWN_TICKS: u32 = 36_000;
/// Most pairs cooling down at once ([`DestructionImpact::cooling_pairs`] never exceeds it,
/// ADR-7431). Past it the cooldowns that end soonest are dropped first (ties in pair order), so
/// those pairs may hit again early; without the limit the list could hold `max_hits_per_tick ×
/// cooldown_ticks` pairs (147 million at the setting limits) and every contact searched it.
pub const MAX_IMPACT_COOLDOWNS: usize = 16_384;

/// How contacts turn into hits (one set for every enabled structure of a world).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ImpactSettings {
    /// Contact speed (m/s) below which a contact does no damage: resting and pushing contacts.
    /// The contact speed is the impulse over the pair's reduced linear mass.
    pub min_speed: f32,
    /// Ticks after a hit during which the same pair (chunk collider, other collider) cannot hit
    /// again: one crash is one hit per chunk, not one per tick of contact.
    pub cooldown_ticks: u32,
    /// Most hits queued per tick over every structure; the strongest win, the rest are counted in
    /// [`ImpactReport::dropped_by_cap`].
    pub max_hits_per_tick: u32,
    /// Whether debris (detached chunks, falling sections) and other structures hitting a structure
    /// damage it. Off by default: breaks do not cascade through falling pieces.
    pub debris_hits: bool,
    /// Multiplier on the contact impulse before the material's `damage_scale` (tune how hard
    /// contacts hit without touching the asset).
    pub impulse_scale: f32,
}

impl Default for ImpactSettings {
    fn default() -> Self {
        Self {
            min_speed: 2.0,
            cooldown_ticks: 10,
            max_hits_per_tick: 32,
            debris_hits: false,
            impulse_scale: 1.0,
        }
    }
}

impl ImpactSettings {
    /// Checks the ranges: `min_speed` and `impulse_scale` finite and `>= 0`,
    /// `max_hits_per_tick <= MAX_IMPACT_HITS_PER_TICK`,
    /// `cooldown_ticks <= MAX_IMPACT_COOLDOWN_TICKS`.
    ///
    /// # Errors
    /// [`ImpactError::Invalid`] naming the first bad field.
    pub fn validate(&self) -> Result<(), ImpactError> {
        if !(self.min_speed.is_finite() && self.min_speed >= 0.0) {
            return Err(ImpactError::Invalid("min_speed must be finite and >= 0".into()));
        }
        if !(self.impulse_scale.is_finite() && self.impulse_scale >= 0.0) {
            return Err(ImpactError::Invalid(
                "impulse_scale must be finite and >= 0".into(),
            ));
        }
        if self.max_hits_per_tick > MAX_IMPACT_HITS_PER_TICK {
            return Err(ImpactError::Invalid(format!(
                "max_hits_per_tick must be <= {MAX_IMPACT_HITS_PER_TICK}"
            )));
        }
        if self.cooldown_ticks > MAX_IMPACT_COOLDOWN_TICKS {
            return Err(ImpactError::Invalid(format!(
                "cooldown_ticks must be <= {MAX_IMPACT_COOLDOWN_TICKS}"
            )));
        }
        Ok(())
    }
}

/// Errors of the impact-damage API. [`ImpactError::code`] is the agent-protocol error code.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ImpactError {
    /// A setting is out of range.
    #[error("invalid impact setting: {0}")]
    Invalid(String),
    /// No instance has this id.
    #[error("no destructible with id {0}")]
    UnknownId(u32),
}

impl ImpactError {
    /// The agent-protocol error code: lowercase ASCII, digits and `_`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Invalid(_) => "destruction_impact_invalid",
            Self::UnknownId(_) => "destruction_unknown_id",
        }
    }
}

/// One hit queued from a contact.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ImpactHit {
    /// The structure.
    pub destructible: DestructibleId,
    /// The chunk under the contact.
    pub chunk: u32,
    /// Impulse queued, newton-seconds (the contact impulse times `impulse_scale`; the material's
    /// `damage_scale` applies in the break model).
    pub impulse: f32,
    /// The contact force, newtons (the contact impulse over the step).
    pub force: f32,
    /// Contact speed, m/s (the contact impulse over the pair's reduced linear mass).
    pub speed: f32,
    /// World-space contact point.
    pub point: [f32; 3],
    /// Unit direction of the impulse, into the chunk.
    pub direction: [f32; 3],
    /// The other collider was debris or another structure (only with `debris_hits`).
    pub debris: bool,
    /// The hit's damage queue number ([`DestructionWorld::damage`]).
    pub queue: u64,
}

/// What the last [`DestructionImpact::convert`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImpactReport {
    /// The destruction tick it ran after.
    pub tick: u64,
    /// Contact pairs with an impulse on enabled structures.
    pub pairs: u32,
    /// Hits queued.
    pub hits: u32,
    /// Pairs that passed every gate but did not fit under `max_hits_per_tick`.
    pub dropped_by_cap: u32,
    /// Pairs below the material's `min_impulse`.
    pub below_min_impulse: u32,
    /// Pairs below `min_speed`.
    pub below_speed: u32,
    /// Pairs still cooling down from an earlier hit.
    pub cooling_down: u32,
    /// Pairs whose other side is debris or another structure (with `debris_hits` off).
    pub debris_skipped: u32,
    /// Hits the world refused (none expected: the targets come from the world).
    pub rejected: u32,
    /// Nothing was converted because this world is a replica (its server converts).
    pub replica: bool,
}

/// Running totals of [`DestructionImpact`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImpactTotals {
    /// Conversions that read the narrow phase.
    pub ticks: u64,
    /// Contact pairs measured.
    pub pairs: u64,
    /// Hits queued.
    pub hits: u64,
    /// Dropped by the per-tick cap.
    pub dropped_by_cap: u64,
    /// Below the material's `min_impulse`.
    pub below_min_impulse: u64,
    /// Below `min_speed`.
    pub below_speed: u64,
    /// Cooling down.
    pub cooling_down: u64,
    /// Debris or structure contacts skipped.
    pub debris_skipped: u64,
}

/// A pair's cooldown: `(structure collider bits, other collider bits)` cannot hit through `until`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Cooldown {
    key: (u64, u64),
    until: u64,
}

/// One measured pair of one chunk collider.
#[derive(Clone, Copy, Debug)]
struct PairMeasure {
    other: u64,
    impulse: f32,
    speed: f32,
    point: Vec3,
    direction: Vec3,
    debris: bool,
}

/// A pair that passed the gates, before the cap.
#[derive(Clone, Copy, Debug)]
struct Candidate {
    id: DestructibleId,
    chunk: u32,
    collider: u64,
    /// The contact impulse times `impulse_scale`.
    scaled: f32,
    pair: PairMeasure,
}

/// Impact damage of one destruction world: the settings, the opted-in structures and the buffers
/// kept between ticks. [`super::DestructionPlugin`] inserts it and runs [`convert_impacts`] after
/// the physics step.
#[derive(Resource, Clone, Debug, Default)]
pub struct DestructionImpact {
    settings: ImpactSettings,
    enabled: BTreeSet<DestructibleId>,
    cooldowns: Vec<Cooldown>,
    measures: Vec<PairMeasure>,
    candidates: Vec<Candidate>,
    hits: Vec<ImpactHit>,
    report: ImpactReport,
    totals: ImpactTotals,
}

/// A collider handle as one sortable number (generation high, index low).
fn handle_bits(h: ColliderHandle) -> u64 {
    let (index, generation) = h.into_raw_parts();
    (u64::from(generation) << 32) | u64::from(index)
}

/// Inverse linear mass of a collider's body: 0 for fixed, kinematic or parentless colliders.
fn inverse_mass(physics: &PhysicsWorld, collider: ColliderHandle) -> f32 {
    let Some(body) = physics
        .colliders
        .get(collider)
        .and_then(|c| c.parent())
        .and_then(|b| physics.bodies.get(b))
    else {
        return 0.0;
    };
    let mass = body.mass();
    if body.is_dynamic() && mass.is_finite() && mass > 0.0 {
        1.0 / mass
    } else {
        0.0
    }
}

/// Reads the contact pairs of one intact chunk collider into `out` (read only, the narrow phase of
/// the last step): one [`PairMeasure`] per touching pair with an impulse, sorted by the other
/// collider's handle bits.
fn measure_pairs(
    physics: &PhysicsWorld,
    bridge: &DestructionPhysics,
    collider: ColliderHandle,
    centroid: Vec3,
    out: &mut Vec<PairMeasure>,
) {
    out.clear();
    let Some(own) = physics.colliders.get(collider) else {
        return;
    };
    let (t, r) = from_rapier_pose(own.position());
    let own_inverse = inverse_mass(physics, collider);
    for pair in physics.narrow_phase.contact_pairs_with(collider) {
        if !pair.has_any_active_contact() {
            continue;
        }
        let impulse = pair.total_impulse_magnitude();
        if !(impulse.is_finite() && impulse > 0.0) {
            continue;
        }
        let ours_first = pair.collider1 == collider;
        let other = if ours_first {
            pair.collider2
        } else {
            pair.collider1
        };
        // The contact point: impulse-weighted mean of the points on our side; the plain mean when
        // the solver kept the impulses in contact clusters.
        let mut weighted = Vec3::ZERO;
        let mut weight = 0.0f32;
        let mut plain = Vec3::ZERO;
        let mut count = 0u32;
        for manifold in pair.manifolds() {
            for point in &manifold.points {
                let local = if ours_first {
                    point.local_p1
                } else {
                    point.local_p2
                };
                let world = t + r * from_rapier_vec3(local);
                plain += world;
                count += 1;
                let w = point.data.impulse.max(0.0);
                weighted += world * w;
                weight += w;
            }
        }
        let point = if weight > 0.0 {
            weighted / weight
        } else if count > 0 {
            plain / count as f32
        } else {
            centroid
        };
        // Into the chunk: the pair's impulse vector, turned towards the chunk's centre.
        let into = centroid - point;
        let mut direction = from_rapier_vec3(pair.total_impulse()).normalize_or_zero();
        if direction.dot(into) < 0.0 {
            direction = -direction;
        }
        if direction == Vec3::ZERO {
            direction = into.normalize_or_zero();
        }
        if direction == Vec3::ZERO {
            direction = Vec3::NEG_Y;
        }
        let inverse = own_inverse + inverse_mass(physics, other);
        let speed = if inverse > 0.0 { impulse * inverse } else { 0.0 };
        out.push(PairMeasure {
            other: handle_bits(other),
            impulse,
            speed: if speed.is_finite() { speed } else { 0.0 },
            point,
            direction,
            debris: bridge.chunk_of_collider(other).is_some(),
        });
    }
    out.sort_unstable_by_key(|m| m.other);
}

impl DestructionImpact {
    /// Impact damage with `settings` and no structure enabled.
    ///
    /// # Errors
    /// [`ImpactError::Invalid`] for a setting out of range.
    pub fn new(settings: ImpactSettings) -> Result<Self, ImpactError> {
        settings.validate()?;
        Ok(Self {
            settings,
            ..Self::default()
        })
    }

    /// The settings.
    #[must_use]
    pub fn settings(&self) -> &ImpactSettings {
        &self.settings
    }

    /// Replaces the settings (nothing changes on an error). Cooldowns already running keep their
    /// end tick.
    ///
    /// # Errors
    /// [`ImpactError::Invalid`] for a setting out of range.
    pub fn set_settings(&mut self, settings: ImpactSettings) -> Result<(), ImpactError> {
        settings.validate()?;
        self.settings = settings;
        Ok(())
    }

    /// Opts structure `id` in: its contacts become hits from the next conversion. Returns whether it
    /// was off before.
    ///
    /// # Errors
    /// [`ImpactError::UnknownId`] when `world` has no such instance.
    pub fn enable(&mut self, world: &DestructionWorld, id: DestructibleId) -> Result<bool, ImpactError> {
        if world.instance(id).is_none() {
            return Err(ImpactError::UnknownId(id.0));
        }
        Ok(self.enabled.insert(id))
    }

    /// Opts structure `id` out (its running cooldowns run out on their own). Returns whether it
    /// was on.
    pub fn disable(&mut self, id: DestructibleId) -> bool {
        self.enabled.remove(&id)
    }

    /// Whether structure `id` is opted in.
    #[must_use]
    pub fn is_enabled(&self, id: DestructibleId) -> bool {
        self.enabled.contains(&id)
    }

    /// The opted-in structures in id order.
    pub fn enabled(&self) -> impl Iterator<Item = DestructibleId> + '_ {
        self.enabled.iter().copied()
    }

    /// What the last conversion did.
    #[must_use]
    pub fn last_report(&self) -> &ImpactReport {
        &self.report
    }

    /// The hits the last conversion queued, strongest first.
    #[must_use]
    pub fn last_hits(&self) -> &[ImpactHit] {
        &self.hits
    }

    /// Running totals.
    #[must_use]
    pub fn totals(&self) -> &ImpactTotals {
        &self.totals
    }

    /// Pairs cooling down now, at most [`MAX_IMPACT_COOLDOWNS`].
    #[must_use]
    pub fn cooling_pairs(&self) -> usize {
        self.cooldowns.len()
    }

    /// Drops the cooldowns that ran out before `tick` and sorts the rest by pair, for
    /// [`Self::is_cooling`]. Allocation-free.
    fn expire_cooldowns(&mut self, tick: u64) {
        self.cooldowns.retain(|c| c.until >= tick);
        self.cooldowns.sort_unstable_by_key(|c| c.key);
    }

    /// Whether `key` is cooling down: a binary search over the list [`Self::expire_cooldowns`]
    /// sorted this tick (the hits of this tick are appended after every lookup).
    fn is_cooling(&self, key: (u64, u64)) -> bool {
        self.cooldowns.binary_search_by_key(&key, |c| c.key).is_ok()
    }

    /// Keeps at most [`MAX_IMPACT_COOLDOWNS`]: drops the ones that end soonest (ties in pair
    /// order), once per tick after its hits.
    fn bound_cooldowns(&mut self) {
        let excess = self.cooldowns.len().saturating_sub(MAX_IMPACT_COOLDOWNS);
        if excess > 0 {
            self.cooldowns.sort_unstable_by_key(|c| (c.until, c.key));
            self.cooldowns.drain(..excess);
        }
    }

    /// Turns the last physics step's contacts on enabled structures into hits queued on `world`
    /// (see the module docs for the rules). Call it after the physics step and before the next
    /// [`DestructionWorld::step`]; `dt` is the physics step in seconds. A replica world converts
    /// nothing ([`ImpactReport::replica`]); with no structure enabled nothing is read.
    pub fn convert(
        &mut self,
        physics: &PhysicsWorld,
        bridge: &DestructionPhysics,
        world: &mut DestructionWorld,
        dt: f32,
    ) -> &ImpactReport {
        let tick = world.tick();
        self.report = ImpactReport {
            tick,
            ..ImpactReport::default()
        };
        self.hits.clear();
        self.candidates.clear();
        // Structures removed since they were enabled.
        self.enabled.retain(|id| world.instance(*id).is_some());
        if self.enabled.is_empty() {
            self.cooldowns.clear();
            return &self.report;
        }
        if world.authority() != Authority::Server {
            self.report.replica = true;
            return &self.report;
        }
        let _span = tracing::debug_span!(target: "regia_physics", "destruction_impact_convert").entered();
        self.totals.ticks += 1;
        self.expire_cooldowns(tick);
        let settings = &self.settings;
        let mut report = self.report;
        for &id in &self.enabled {
            let Some(inst) = world.instance(id) else {
                continue;
            };
            let material = &inst.asset().material;
            for chunk in 0..inst.asset().chunks.len() as u32 {
                let Some(collider) = bridge.intact_collider(id, chunk) else {
                    continue;
                };
                let centroid = inst.world_centroid(chunk).map_or(Vec3::ZERO, Vec3::from_array);
                measure_pairs(physics, bridge, collider, centroid, &mut self.measures);
                let collider = handle_bits(collider);
                for m in &self.measures {
                    report.pairs += 1;
                    if m.debris && !settings.debris_hits {
                        report.debris_skipped += 1;
                        continue;
                    }
                    let scaled = m.impulse * settings.impulse_scale;
                    let damage = scaled * material.damage_scale;
                    if !(damage > 0.0 && damage >= material.min_impulse) {
                        report.below_min_impulse += 1;
                        continue;
                    }
                    if m.speed < settings.min_speed {
                        report.below_speed += 1;
                        continue;
                    }
                    if self.is_cooling((collider, m.other)) {
                        report.cooling_down += 1;
                        continue;
                    }
                    self.candidates.push(Candidate {
                        id,
                        chunk,
                        collider,
                        scaled,
                        pair: *m,
                    });
                }
            }
        }
        // Strongest first; ties in instance, chunk and handle order.
        self.candidates.sort_unstable_by(|a, b| {
            b.scaled
                .total_cmp(&a.scaled)
                .then(a.id.cmp(&b.id))
                .then(a.chunk.cmp(&b.chunk))
                .then(a.pair.other.cmp(&b.pair.other))
        });
        let cap = settings.max_hits_per_tick as usize;
        report.dropped_by_cap = self.candidates.len().saturating_sub(cap) as u32;
        let force_scale = if dt > 0.0 { 1.0 / dt } else { 0.0 };
        for c in self.candidates.iter().take(cap) {
            let impulse = c.pair.direction * c.scaled;
            let queued = world.damage(Damage {
                target: Some(c.id),
                kind: DamageKind::Impulse {
                    chunk: c.chunk,
                    impulse: impulse.to_array(),
                },
            });
            let Ok(queue) = queued else {
                report.rejected += 1;
                continue;
            };
            report.hits += 1;
            self.hits.push(ImpactHit {
                destructible: c.id,
                chunk: c.chunk,
                impulse: c.scaled,
                force: c.pair.impulse * force_scale,
                speed: c.pair.speed,
                point: c.pair.point.to_array(),
                direction: c.pair.direction.to_array(),
                debris: c.pair.debris,
                queue,
            });
            self.cooldowns.push(Cooldown {
                key: (c.collider, c.pair.other),
                until: tick + u64::from(settings.cooldown_ticks),
            });
        }
        self.bound_cooldowns();
        self.totals.pairs += u64::from(report.pairs);
        self.totals.hits += u64::from(report.hits);
        self.totals.dropped_by_cap += u64::from(report.dropped_by_cap);
        self.totals.below_min_impulse += u64::from(report.below_min_impulse);
        self.totals.below_speed += u64::from(report.below_speed);
        self.totals.cooling_down += u64::from(report.cooling_down);
        self.totals.debris_skipped += u64::from(report.debris_skipped);
        self.report = report;
        &self.report
    }
}

/// Runs [`DestructionImpact::convert`] on the app's [`Destruction`] after the physics step
/// (`FixedUpdate`, after `PhysicsStep`); the hits apply at the next `step_destruction`. Returns at
/// once when no structure is enabled.
pub fn convert_impacts(
    fixed: Res<FixedTime>,
    impact: Option<ResMut<DestructionImpact>>,
    destruction: Option<ResMut<Destruction>>,
    physics: Option<Res<PhysicsWorld>>,
) {
    let (Some(mut impact), Some(mut destruction), Some(physics)) = (impact, destruction, physics) else {
        return;
    };
    if impact.enabled.is_empty() {
        return;
    }
    let dt = fixed.step().as_secs_f32();
    let d = &mut *destruction;
    impact.convert(&physics, &d.physics, &mut d.world, dt);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_are_validated_and_codes_are_protocol_safe() {
        assert_eq!(ImpactSettings::default().validate(), Ok(()));
        for bad in [
            ImpactSettings {
                min_speed: f32::NAN,
                ..ImpactSettings::default()
            },
            ImpactSettings {
                min_speed: -1.0,
                ..ImpactSettings::default()
            },
            ImpactSettings {
                impulse_scale: f32::INFINITY,
                ..ImpactSettings::default()
            },
            ImpactSettings {
                max_hits_per_tick: MAX_IMPACT_HITS_PER_TICK + 1,
                ..ImpactSettings::default()
            },
            ImpactSettings {
                cooldown_ticks: MAX_IMPACT_COOLDOWN_TICKS + 1,
                ..ImpactSettings::default()
            },
        ] {
            let e = bad.validate().expect_err("out of range");
            assert_eq!(e.code(), "destruction_impact_invalid");
            assert!(DestructionImpact::new(bad).is_err());
        }
        for code in [
            ImpactError::Invalid(String::new()).code(),
            ImpactError::UnknownId(0).code(),
        ] {
            assert!(
                code.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            );
        }
    }

    #[test]
    fn enabling_needs_a_known_instance_and_a_replica_converts_nothing() {
        let mut world = DestructionWorld::new(Authority::Replica);
        let mut impact = DestructionImpact::default();
        assert_eq!(
            impact.enable(&world, DestructibleId(0)),
            Err(ImpactError::UnknownId(0))
        );
        let asset = std::sync::Arc::new(
            super::super::DestructibleAsset::box_grid(&super::super::BoxGrid {
                name: "w".into(),
                counts: [2, 2, 1],
                cell: [1.0; 3],
                density: 100.0,
                strength_per_area: 100.0,
                support: super::super::GridSupport::Bottom,
                material: super::super::DestructionMaterial::default(),
            })
            .expect("grid"),
        );
        let id = world
            .define(asset, super::super::DestructiblePose::default(), 1)
            .expect("define");
        assert_eq!(impact.enable(&world, id), Ok(true));
        assert_eq!(impact.enable(&world, id), Ok(false));
        let physics = PhysicsWorld::default();
        let bridge = DestructionPhysics::default();
        let r = *impact.convert(&physics, &bridge, &mut world, 1.0 / 60.0);
        assert!(r.replica);
        assert_eq!(r.hits, 0);
        assert_eq!(world.queued_damage(), 0);
        assert!(impact.disable(id));
        assert!(!impact.is_enabled(id));
        // A structure removed while enabled leaves the list at the next conversion.
        assert_eq!(impact.enable(&world, id), Ok(true));
        assert!(world.remove(id));
        impact.convert(&physics, &bridge, &mut world, 1.0 / 60.0);
        assert_eq!(impact.enabled().count(), 0);
    }

    /// ADR-7431 (5): the cooldown list has a limit. Saturated ticks (the most hits a tick, the
    /// longest cooldown) keep it at `MAX_IMPACT_COOLDOWNS`, dropping the cooldowns that end
    /// soonest; a pair still listed is found by the sorted lookup, a dropped one is not.
    #[test]
    fn the_cooldown_list_stays_under_its_limit() {
        let mut impact = DestructionImpact::default();
        let per_tick = MAX_IMPACT_HITS_PER_TICK as u64;
        let mut most = 0;
        for tick in 0..8u64 {
            impact.expire_cooldowns(tick);
            for n in 0..per_tick {
                impact.cooldowns.push(Cooldown {
                    key: (tick, n),
                    until: tick + u64::from(MAX_IMPACT_COOLDOWN_TICKS),
                });
            }
            impact.bound_cooldowns();
            most = most.max(impact.cooling_pairs());
            assert!(impact.cooling_pairs() <= MAX_IMPACT_COOLDOWNS, "tick {tick}");
        }
        assert_eq!(most, MAX_IMPACT_COOLDOWNS, "the limit is reached and held");
        impact.expire_cooldowns(8);
        // 8 ticks of 4096 pushed, 16384 kept: the last four ticks (the latest `until`).
        assert!(impact.is_cooling((7, 0)) && impact.is_cooling((4, 4095)));
        assert!(!impact.is_cooling((3, 4095)) && !impact.is_cooling((0, 0)));
        // Running out still works: past every `until` the list empties.
        impact.expire_cooldowns(8 + u64::from(MAX_IMPACT_COOLDOWN_TICKS) + 1);
        assert_eq!(impact.cooling_pairs(), 0);
    }
}
