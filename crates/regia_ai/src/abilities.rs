// SPDX-License-Identifier: MPL-2.0
//! Cooldown- and cost-gated AI abilities (`docs/specs/22-tier2.md` § T2.2).
//!
//! A behaviour-tree leaf (or a script) triggers a named ability on an agent through the
//! [`Abilities`] component. Each [`Ability`] pairs an authored [`AbilityDef`] (cooldown seconds,
//! resource cost) with runtime state (remaining cooldown, activation count). [`crate::AiPlugin`]
//! ticks every set once per fixed step ([`tick_abilities`], in [`crate::AiStep`] before the
//! behavior trees); activation checks the cooldown first, then the resource pool, so a rejected
//! activation never spends resource or starts a cooldown. Agents read and edit the component
//! through the generic `scene.*` calls (`book/agents/ai.md`, ADR-1563).
//!
//! Gating beyond cooldown and cost (blackboard keys, tags, line of sight) stays with the caller:
//! check it with a `Condition` node or a tag query before calling [`Abilities::try_activate`].
//! Iteration order is sorted by ability id ([`BTreeMap`]), so ticks and [`Abilities::ready_ids`]
//! are deterministic for replays.

use std::collections::BTreeMap;

use regia_app::prelude::*;
use regia_scene::layers::LayerParked;
use thiserror::Error;

/// Reads an authored duration, cost, stored cooldown or resource pool as a well-formed value:
/// only finite, positive values pass through; everything else (negative, NaN, infinite) reads as
/// `0.0`. The stored field is not rewritten here, so a reflected edit can still hold a NaN; every
/// read the tick and activation use is `0.0`, and a negative `remaining` is written back as `0.0`
/// on the next ability tick.
fn non_negative(value: f32) -> f32 {
    if value.is_finite() && value > 0.0 {
        value
    } else {
        0.0
    }
}

/// The authored shape of one ability: its id, cooldown and resource cost.
#[derive(Reflect, Debug, Clone, PartialEq)]
pub struct AbilityDef {
    /// Ability id (the lookup key; empty ids are rejected by [`Abilities::add`]).
    pub id: String,
    /// Cooldown in seconds started by each activation (non-finite/negative reads as `0.0`).
    pub cooldown_secs: f32,
    /// Resource (mana, stamina, ...) spent by each activation (non-finite/negative reads as `0.0`).
    pub cost: f32,
}

impl Default for AbilityDef {
    /// An empty, free, cooldown-less definition.
    fn default() -> Self {
        Self {
            id: String::new(),
            cooldown_secs: 0.0,
            cost: 0.0,
        }
    }
}

impl AbilityDef {
    /// Authors a definition; `cooldown_secs`/`cost` are sanitized at use time, not here, so a
    /// reflected edit holding a bad value behaves the same as a constructed one.
    pub fn new(id: impl Into<String>, cooldown_secs: f32, cost: f32) -> Self {
        Self {
            id: id.into(),
            cooldown_secs,
            cost,
        }
    }

    /// The effective cooldown: finite and positive, else `0.0`.
    pub fn effective_cooldown(&self) -> f32 {
        non_negative(self.cooldown_secs)
    }

    /// The effective cost: finite and positive, else `0.0`.
    pub fn effective_cost(&self) -> f32 {
        non_negative(self.cost)
    }
}

/// Why an activation was rejected.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum AbilityError {
    /// No ability with this id is registered on the entity.
    #[error("unknown ability {0:?}")]
    Unknown(String),
    /// The ability is cooling down; `remaining` is the seconds left.
    #[error("ability {id:?} is on cooldown ({remaining:.2}s left)")]
    OnCooldown {
        /// The ability id.
        id: String,
        /// Seconds left on the cooldown.
        remaining: f32,
    },
    /// The resource pool holds less than the ability costs.
    #[error("ability {id:?} needs {need} resource, has {have}")]
    Insufficient {
        /// The ability id.
        id: String,
        /// The sanitized cost.
        need: f32,
        /// The (sanitized) pool contents.
        have: f32,
    },
}

/// One ability slot: the authored [`AbilityDef`] plus its runtime state.
#[derive(Reflect, Debug, Clone, PartialEq)]
pub struct Ability {
    /// The authored definition.
    pub def: AbilityDef,
    /// Seconds left on the cooldown (`0.0` means ready).
    pub remaining: f32,
    /// Successful activations so far (for debugging and agent-protocol inspection).
    pub activations: u64,
}

impl Default for Ability {
    /// A default-defined, ready ability with no activations.
    fn default() -> Self {
        Self {
            def: AbilityDef::default(),
            remaining: 0.0,
            activations: 0,
        }
    }
}

impl Ability {
    /// A ready slot for `def`.
    pub fn new(def: AbilityDef) -> Self {
        Self {
            def,
            remaining: 0.0,
            activations: 0,
        }
    }

    /// Whether the ability can fire right now (no remaining cooldown).
    pub fn is_ready(&self) -> bool {
        non_negative(self.remaining) <= 0.0
    }

    /// Seconds left on the cooldown (`0.0` when ready; never non-finite or negative).
    pub fn remaining_secs(&self) -> f32 {
        non_negative(self.remaining)
    }

    /// Advances the cooldown by `dt` seconds; non-finite or non-positive `dt` is a no-op.
    pub fn tick(&mut self, dt: f32) {
        self.remaining = advanced_cooldown(self.remaining, dt);
    }

    /// Checks whether the ability would fire against a pool holding `have`, without mutating.
    pub fn can_activate(&self, have: f32) -> Result<(), AbilityError> {
        let remaining = non_negative(self.remaining);
        if remaining > 0.0 {
            return Err(AbilityError::OnCooldown {
                id: self.def.id.clone(),
                remaining,
            });
        }
        let need = self.def.effective_cost();
        let have = non_negative(have);
        if have < need {
            return Err(AbilityError::Insufficient {
                id: self.def.id.clone(),
                need,
                have,
            });
        }
        Ok(())
    }

    /// Fires the ability: spends `resource` and starts the cooldown. Checked in the same order
    /// as [`Ability::can_activate`], so a rejection leaves both untouched.
    pub fn try_activate(&mut self, resource: &mut f32) -> Result<(), AbilityError> {
        self.can_activate(*resource)?;
        let need = self.def.effective_cost();
        let have = non_negative(*resource);
        *resource = (have - need).max(0.0);
        self.remaining = self.def.effective_cooldown();
        self.activations = self.activations.saturating_add(1);
        Ok(())
    }

    /// Cancels a running cooldown, making the ability ready again.
    pub fn cancel(&mut self) {
        self.remaining = 0.0;
    }
}

/// A cooldown remainder at or below this many seconds counts as done. Summing an `f32` fixed
/// step leaves rounding residue (60 steps of 1/60 s miss 1.0 by a few millionths), which would
/// otherwise make a cooldown ready one step late; 0.1 ms is far below any fixed step.
const COOLDOWN_EPSILON: f32 = 1.0e-4;

/// Advances a stored cooldown by `dt`, returning a well-formed remainder.
fn advanced_cooldown(remaining: f32, dt: f32) -> f32 {
    if !dt.is_finite() || dt <= 0.0 {
        return non_negative(remaining);
    }
    let left = non_negative(remaining) - dt;
    if left <= COOLDOWN_EPSILON { 0.0 } else { left }
}

/// The ability set of one AI agent: cooldown- and cost-gated actions keyed by ability id.
///
/// Attach alongside a [`crate::BehaviorTree`] and [`crate::Blackboard`]; [`crate::AiPlugin`]
/// ticks it once per fixed step ([`tick_abilities`]), and a leaf task or script fires it with
/// [`Abilities::try_activate`].
#[derive(Component, Reflect, Debug, Clone, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct Abilities {
    /// Slots by ability id (sorted, so ticks and listings are deterministic).
    pub slots: BTreeMap<String, Ability>,
}

impl Abilities {
    /// An empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `def` as a ready slot, replacing any slot with the same id. Returns `false`
    /// (and stores nothing) when `def.id` is empty.
    pub fn add(&mut self, def: AbilityDef) -> bool {
        if def.id.is_empty() {
            return false;
        }
        self.slots.insert(def.id.clone(), Ability::new(def));
        true
    }

    /// Whether an ability with `id` is registered.
    pub fn contains(&self, id: &str) -> bool {
        self.slots.contains_key(id)
    }

    /// Reads a slot by id.
    pub fn get(&self, id: &str) -> Option<&Ability> {
        self.slots.get(id)
    }

    /// How many slots are registered.
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// Whether no slots are registered.
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Seconds left on `id` (`0.0` when ready or unknown).
    pub fn remaining(&self, id: &str) -> f32 {
        self.slots.get(id).map_or(0.0, Ability::remaining_secs)
    }

    /// Whether `id` is registered and ready.
    pub fn is_ready(&self, id: &str) -> bool {
        self.slots.get(id).is_some_and(Ability::is_ready)
    }

    /// Ids of the ready slots, in sorted order.
    pub fn ready_ids(&self) -> Vec<String> {
        self.slots
            .iter()
            .filter(|(_, slot)| slot.is_ready())
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Fires `id`, spending `resource` and starting the cooldown.
    pub fn try_activate(&mut self, id: &str, resource: &mut f32) -> Result<(), AbilityError> {
        match self.slots.get_mut(id) {
            Some(slot) => slot.try_activate(resource),
            None => Err(AbilityError::Unknown(id.to_string())),
        }
    }

    /// Cancels the cooldown on `id`. Returns `false` when `id` is unknown.
    pub fn cancel(&mut self, id: &str) -> bool {
        match self.slots.get_mut(id) {
            Some(slot) => {
                slot.cancel();
                true
            }
            None => false,
        }
    }

    /// Advances every cooldown by `dt` seconds (one fixed step when driven by the AI tick).
    pub fn tick(&mut self, dt: f32) {
        for slot in self.slots.values_mut() {
            slot.tick(dt);
        }
    }
}

/// `FixedUpdate` (exclusive, in [`crate::AiStep`] before the behavior-tree tick): advances every
/// [`Abilities`] cooldown by the fixed step, never the frame delta, so cooldowns are the same at
/// any frame rate and in replays (ADR-537, ADR-867).
pub fn tick_abilities(world: &mut World) {
    let mut query = world.query_filtered::<&mut Abilities, Without<LayerParked>>();
    tick_abilities_with(world, &mut query);
}

/// [`tick_abilities`] with a query kept between steps: the system [`crate::AiPlugin`] runs.
#[tracing::instrument(skip_all, target = "regia_ai", name = "abilities_tick")]
pub(crate) fn tick_abilities_with(
    world: &mut World,
    query: &mut QueryState<&mut Abilities, Without<LayerParked>>,
) {
    let dt = world
        .get_resource::<FixedTime>()
        .map_or(1.0 / 60.0, |fixed| fixed.step().as_secs_f32());
    for mut set in query.iter_mut(world) {
        set.tick(dt);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fixed_step_system_ticks_every_set() {
        let mut world = World::new();
        let mut set = Abilities::new();
        assert!(set.add(AbilityDef::new("dash", 1.0, 0.0)));
        let mut pool = 0.0;
        assert!(set.try_activate("dash", &mut pool).is_ok());
        let agent = world.spawn(set).id();
        // No `FixedTime`: the 60 Hz default step.
        for _ in 0..30 {
            tick_abilities(&mut world);
        }
        let left = world
            .get::<Abilities>(agent)
            .map_or(-1.0, |set| set.remaining("dash"));
        assert!((left - 0.5).abs() < 1.0e-4, "half the cooldown left, got {left}");
        for _ in 0..30 {
            tick_abilities(&mut world);
        }
        assert!(
            world
                .get::<Abilities>(agent)
                .is_some_and(|set| set.is_ready("dash"))
        );
    }

    fn set_with_dash() -> Abilities {
        let mut set = Abilities::new();
        assert!(set.add(AbilityDef::new("dash", 2.0, 10.0)));
        set
    }

    #[test]
    fn activation_spends_cost_and_starts_cooldown() {
        let mut set = set_with_dash();
        let mut pool = 25.0;
        assert!(set.try_activate("dash", &mut pool).is_ok());
        assert_eq!(pool, 15.0);
        assert_eq!(set.remaining("dash"), 2.0);
        assert!(!set.is_ready("dash"));
        assert_eq!(set.get("dash").map_or(0, |slot| slot.activations), 1);
    }

    #[test]
    fn cooldown_blocks_then_tick_recovers() {
        let mut set = set_with_dash();
        let mut pool = 100.0;
        assert!(set.try_activate("dash", &mut pool).is_ok());
        let blocked = set.try_activate("dash", &mut pool);
        match blocked {
            Err(AbilityError::OnCooldown { ref id, remaining }) => {
                assert_eq!(id, "dash");
                assert!(remaining > 0.0 && remaining <= 2.0);
            }
            other => panic!("expected OnCooldown, got {other:?}"),
        }
        assert_eq!(pool, 90.0);
        set.tick(1.0);
        assert!(!set.is_ready("dash"));
        set.tick(1.0);
        assert!(set.is_ready("dash"));
        assert!(set.try_activate("dash", &mut pool).is_ok());
        assert_eq!(pool, 80.0);
    }

    #[test]
    fn insufficient_resource_rejected_without_side_effects() {
        let mut set = set_with_dash();
        let mut pool = 5.0;
        let err = set
            .try_activate("dash", &mut pool)
            .expect_err("poor pool must be rejected");
        match err {
            AbilityError::Insufficient { ref id, need, have } => {
                assert_eq!(id, "dash");
                assert_eq!(need, 10.0);
                assert_eq!(have, 5.0);
            }
            other => panic!("expected Insufficient, got {other:?}"),
        }
        assert_eq!(pool, 5.0);
        assert!(set.is_ready("dash"));
    }

    #[test]
    fn unknown_ids_are_rejected() {
        let mut set = set_with_dash();
        let mut pool = 100.0;
        assert_eq!(
            set.try_activate("blink", &mut pool),
            Err(AbilityError::Unknown("blink".to_string()))
        );
        assert!(!set.cancel("blink"));
        assert_eq!(set.remaining("blink"), 0.0);
        assert!(!set.is_ready("blink"));
    }

    #[test]
    fn degenerate_numbers_are_sanitized() {
        let mut set = Abilities::new();
        assert!(!set.add(AbilityDef::new("", 1.0, 1.0)));
        assert!(set.add(AbilityDef::new("free", f32::NAN, f32::NEG_INFINITY)));
        assert!(set.add(AbilityDef::new("fast", -3.0, -5.0)));
        let mut pool = 0.0;
        assert!(set.try_activate("free", &mut pool).is_ok());
        assert!(set.try_activate("fast", &mut pool).is_ok());
        assert!(set.is_ready("free"));
        // A NaN pool spends as empty but never becomes NaN itself.
        let mut nan_pool = f32::NAN;
        assert!(set.try_activate("free", &mut nan_pool).is_ok());
        assert_eq!(nan_pool, 0.0);
        // Non-positive or non-finite ticks never move a cooldown.
        let mut cooldown = set_with_dash();
        let mut pool = 100.0;
        assert!(cooldown.try_activate("dash", &mut pool).is_ok());
        cooldown.tick(0.0);
        cooldown.tick(-1.0);
        cooldown.tick(f32::NAN);
        assert_eq!(cooldown.remaining("dash"), 2.0);
        cooldown.tick(f32::INFINITY);
        assert_eq!(cooldown.remaining("dash"), 2.0);
    }

    #[test]
    fn cancel_resets_the_cooldown() {
        let mut set = set_with_dash();
        let mut pool = 100.0;
        assert!(set.try_activate("dash", &mut pool).is_ok());
        assert!(set.cancel("dash"));
        assert!(set.is_ready("dash"));
        assert_eq!(set.remaining("dash"), 0.0);
        assert!(set.try_activate("dash", &mut pool).is_ok());
    }

    #[test]
    fn ready_ids_lists_only_ready_slots_in_order() {
        let mut set = Abilities::new();
        assert!(set.add(AbilityDef::new("dash", 2.0, 0.0)));
        assert!(set.add(AbilityDef::new("blink", 0.0, 0.0)));
        assert!(set.add(AbilityDef::new("cloak", 5.0, 0.0)));
        let mut pool = 0.0;
        assert!(set.try_activate("dash", &mut pool).is_ok());
        assert_eq!(set.ready_ids(), vec!["blink".to_string(), "cloak".to_string()]);
        set.tick(5.0);
        assert_eq!(
            set.ready_ids(),
            vec!["blink".to_string(), "cloak".to_string(), "dash".to_string()]
        );
    }
}
