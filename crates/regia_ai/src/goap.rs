// SPDX-License-Identifier: MPL-2.0
//! Goal-oriented action planning (GOAP): a forward A\* search over world states that finds a
//! minimum-cost sequence of actions whose effects satisfy a partial goal state.
//!
//! The model follows Jeff Orkin, "Three States and a Plan: The AI of F.E.A.R." (Game
//! Developers Conference Proceedings, 2006): an agent has a set of actions, each with
//! preconditions and effects over a small set of world-state facts; a goal is a partial
//! world state; the planner searches from the current state for a chain of actions reaching
//! any state that satisfies the goal. The search itself is textbook A\* (Hart, Nilsson and
//! Raphael, "A Formal Basis for the Heuristic Determination of Minimum Cost Paths", IEEE
//! Transactions on Systems Science and Cybernetics 4(2), 1968) with an admissible heuristic
//! derived from the unsatisfied goal facts (see [`Planner::plan`]).
//!
//! # World state
//!
//! A [`WorldState`] is a fixed-size bitset of up to [`MAX_BOOL_FACTS`] boolean facts plus up
//! to [`MAX_INT_FACTS`] small integer facts. It is `Copy`, ordered, and hash-free: the search
//! keys states in a [`BTreeMap`], so results never depend on hash
//! iteration order. Facts are addressed by index; a [`FactTable`] maps blackboard key names
//! to fact indices and snapshots a [`Blackboard`] into a `WorldState`.
//!
//! # Example
//!
//! ```
//! use regia_ai::goap::{Action, Goal, PlanOptions, Planner, WorldState};
//!
//! // Fact indices: 0 = has_axe, 1 = has_wood, 2 = has_planks.
//! let planner = Planner::new(vec![
//!     Action::new("pick_up_axe", 1.0).sets(0, true),
//!     Action::new("chop_tree", 2.0).requires(0, true).sets(1, true),
//!     Action::new("saw_planks", 1.0).requires(1, true).sets(2, true),
//! ]);
//! let start = WorldState::new();
//! let goal = Goal::new().require(2, true);
//! let plan = planner
//!     .plan(&start, &goal, &PlanOptions::default())
//!     .expect("a plan exists");
//! let names: Vec<&str> = plan.steps().iter().map(|s| s.name.as_str()).collect();
//! assert_eq!(names, ["pick_up_axe", "chop_tree", "saw_planks"]);
//! assert!(plan.validate(&start));
//! ```

use std::cmp::Ordering;
use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BinaryHeap};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

#[cfg(test)]
thread_local! {
    static CACHE_HIT_HOOK: std::cell::Cell<fn()> = const { std::cell::Cell::new(|| {}) };
}

#[cfg(test)]
fn cache_hit_hook() {
    CACHE_HIT_HOOK.with(|hook| hook.get()());
}

use thiserror::Error;

use crate::blackboard::{BbValue, Blackboard};

/// The maximum number of boolean facts in a [`WorldState`] (two `u64` words).
pub const MAX_BOOL_FACTS: usize = 128;

/// The maximum number of integer facts in a [`WorldState`].
pub const MAX_INT_FACTS: usize = 16;

/// Why a planning request failed.
///
/// The variants are deliberately payload-free so callers can match on them exhaustively and
/// cheaply; validate an action set up front with [`Planner::try_new`] if the distinction
/// between a bad domain and an unreachable goal matters at load time.
#[derive(Error, Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanError {
    /// No sequence of actions reaches the goal from the start state (the search space was
    /// exhausted within the given depth limit without the node limit being the cause).
    #[error("no plan reaches the goal")]
    NoPlan,
    /// The node-expansion limit, or a plan-depth limit that cut off a still-extendable
    /// frontier, was reached before a plan was found. A plan may still exist.
    #[error("planning limit reached")]
    LimitReached,
    /// An action in the planner is malformed: an empty name, a negative or non-finite cost
    /// (including one returned by a dynamic cost callback during the search, or finite
    /// costs summing to a non-finite path cost), or an integer fact index at or beyond
    /// [`MAX_INT_FACTS`].
    #[error("invalid action definition")]
    InvalidAction,
}

/// A set of boolean requirements (or assignments) over fact indices `0..MAX_BOOL_FACTS`.
///
/// Internally a `(mask, values)` pair: a fact participates when its mask bit is set, and
/// `values` then holds the wanted (or assigned) truth value. Used for action preconditions,
/// action effects, and the boolean part of a [`Goal`]. Facts at or beyond
/// [`MAX_BOOL_FACTS`] are ignored by every builder, so a `BoolConditions` is always valid.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BoolConditions {
    /// Which facts participate.
    mask: [u64; 2],
    /// The wanted value of each participating fact (only mask bits are meaningful).
    values: [u64; 2],
}

impl BoolConditions {
    /// An empty condition set (satisfied by every state; as effects, changes nothing).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            mask: [0; 2],
            values: [0; 2],
        }
    }

    /// Builds a condition set from raw masks; `values` bits outside `mask` are cleared so
    /// equality and ordering stay canonical.
    #[must_use]
    pub const fn from_masks(mask: [u64; 2], values: [u64; 2]) -> Self {
        Self {
            mask,
            values: [values[0] & mask[0], values[1] & mask[1]],
        }
    }

    /// The participation mask (fact `i` is bit `i % 64` of word `i / 64`).
    #[must_use]
    pub const fn mask(&self) -> [u64; 2] {
        self.mask
    }

    /// The wanted values of the participating facts.
    #[must_use]
    pub const fn values(&self) -> [u64; 2] {
        self.values
    }

    /// Whether no fact participates.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.mask[0] == 0 && self.mask[1] == 0
    }

    /// How many facts participate.
    #[must_use]
    pub const fn len(&self) -> usize {
        (self.mask[0].count_ones() + self.mask[1].count_ones()) as usize
    }

    /// Adds (or replaces) the requirement that `fact` equals `value`. A `fact` at or beyond
    /// [`MAX_BOOL_FACTS`] is ignored.
    #[must_use]
    pub const fn with(mut self, fact: usize, value: bool) -> Self {
        if fact < MAX_BOOL_FACTS {
            let word = fact / 64;
            let bit = 1u64 << (fact % 64);
            self.mask[word] |= bit;
            if value {
                self.values[word] |= bit;
            } else {
                self.values[word] &= !bit;
            }
        }
        self
    }

    /// The wanted value of `fact`, or `None` when the fact does not participate.
    #[must_use]
    pub const fn get(&self, fact: usize) -> Option<bool> {
        if fact >= MAX_BOOL_FACTS {
            return None;
        }
        let word = fact / 64;
        let bit = 1u64 << (fact % 64);
        if self.mask[word] & bit == 0 {
            None
        } else {
            Some(self.values[word] & bit != 0)
        }
    }

    /// Whether `state` agrees with every participating fact.
    #[must_use]
    pub const fn is_satisfied_by(&self, state: &WorldState) -> bool {
        (state.bits[0] & self.mask[0]) == self.values[0] && (state.bits[1] & self.mask[1]) == self.values[1]
    }

    /// How many participating facts `state` disagrees with.
    #[must_use]
    pub const fn unsatisfied_count(&self, state: &WorldState) -> usize {
        (((state.bits[0] ^ self.values[0]) & self.mask[0]).count_ones()
            + ((state.bits[1] ^ self.values[1]) & self.mask[1]).count_ones()) as usize
    }

    /// Applies these conditions as assignments to `state`.
    const fn apply_to(&self, state: &mut WorldState) {
        state.bits[0] = (state.bits[0] & !self.mask[0]) | self.values[0];
        state.bits[1] = (state.bits[1] & !self.mask[1]) | self.values[1];
    }
}

/// A planner world state: up to [`MAX_BOOL_FACTS`] boolean facts as a bitset plus up to
/// [`MAX_INT_FACTS`] small integer facts.
///
/// A state is total (every fact has a value; unset booleans are `false`, unset integers are
/// `0`), unlike a [`Goal`], which is partial. Out-of-range fact indices never panic: reads
/// return the default and writes report `false`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WorldState {
    /// Boolean facts: fact `i` is bit `i % 64` of word `i / 64`.
    bits: [u64; 2],
    /// Integer facts.
    ints: [i32; MAX_INT_FACTS],
}

impl WorldState {
    /// The empty state: every boolean fact `false`, every integer fact `0`.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            bits: [0; 2],
            ints: [0; MAX_INT_FACTS],
        }
    }

    /// Builds a state from its raw parts.
    #[must_use]
    pub const fn from_parts(bits: [u64; 2], ints: [i32; MAX_INT_FACTS]) -> Self {
        Self { bits, ints }
    }

    /// The raw boolean fact words.
    #[must_use]
    pub const fn bits(&self) -> [u64; 2] {
        self.bits
    }

    /// The raw integer facts.
    #[must_use]
    pub const fn ints(&self) -> &[i32; MAX_INT_FACTS] {
        &self.ints
    }

    /// Reads boolean `fact`; an out-of-range fact reads as `false`.
    #[must_use]
    pub const fn get(&self, fact: usize) -> bool {
        if fact >= MAX_BOOL_FACTS {
            return false;
        }
        self.bits[fact / 64] & (1u64 << (fact % 64)) != 0
    }

    /// Writes boolean `fact`, returning `false` (and changing nothing) when out of range.
    pub const fn set(&mut self, fact: usize, value: bool) -> bool {
        if fact >= MAX_BOOL_FACTS {
            return false;
        }
        let word = fact / 64;
        let bit = 1u64 << (fact % 64);
        if value {
            self.bits[word] |= bit;
        } else {
            self.bits[word] &= !bit;
        }
        true
    }

    /// Builder form of [`WorldState::set`]; an out-of-range fact is ignored.
    #[must_use]
    pub const fn with(mut self, fact: usize, value: bool) -> Self {
        let _ = self.set(fact, value);
        self
    }

    /// Reads integer `fact`; an out-of-range fact reads as `0`.
    #[must_use]
    pub const fn get_int(&self, fact: usize) -> i32 {
        if fact >= MAX_INT_FACTS {
            return 0;
        }
        self.ints[fact]
    }

    /// Writes integer `fact`, returning `false` (and changing nothing) when out of range.
    pub const fn set_int(&mut self, fact: usize, value: i32) -> bool {
        if fact >= MAX_INT_FACTS {
            return false;
        }
        self.ints[fact] = value;
        true
    }

    /// Builder form of [`WorldState::set_int`]; an out-of-range fact is ignored.
    #[must_use]
    pub const fn with_int(mut self, fact: usize, value: i32) -> Self {
        let _ = self.set_int(fact, value);
        self
    }

    /// How many boolean facts are `true`.
    #[must_use]
    pub const fn count_true(&self) -> usize {
        (self.bits[0].count_ones() + self.bits[1].count_ones()) as usize
    }

    /// Whether this state satisfies every condition of `goal`.
    #[must_use]
    pub fn satisfies(&self, goal: &Goal) -> bool {
        goal.is_satisfied_by(self)
    }
}

/// A comparison an integer fact can be tested with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum IntCmp {
    /// `fact == value`
    Eq,
    /// `fact != value`
    Ne,
    /// `fact < value`
    Lt,
    /// `fact <= value`
    Le,
    /// `fact > value`
    Gt,
    /// `fact >= value`
    Ge,
}

impl IntCmp {
    /// Tests `fact_value` against `value` with this comparison.
    #[must_use]
    pub const fn test(&self, fact_value: i32, value: i32) -> bool {
        match self {
            Self::Eq => fact_value == value,
            Self::Ne => fact_value != value,
            Self::Lt => fact_value < value,
            Self::Le => fact_value <= value,
            Self::Gt => fact_value > value,
            Self::Ge => fact_value >= value,
        }
    }

    /// A stable small code for this comparison (used in deterministic cache keys).
    const fn code(&self) -> u8 {
        match self {
            Self::Eq => 0,
            Self::Ne => 1,
            Self::Lt => 2,
            Self::Le => 3,
            Self::Gt => 4,
            Self::Ge => 5,
        }
    }
}

/// One integer-fact condition: `fact <cmp> value`.
///
/// `fact` is a `u8` so conditions stay small; a value at or beyond [`MAX_INT_FACTS`] makes
/// the owning [`Action`] invalid ([`PlanError::InvalidAction`]). The [`Goal`] builders drop
/// out-of-range facts instead, so a goal is always valid.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct IntCondition {
    /// The integer fact index (`0..MAX_INT_FACTS`).
    pub fact: u8,
    /// The comparison to apply.
    pub cmp: IntCmp,
    /// The value to compare against.
    pub value: i32,
}

impl IntCondition {
    /// A condition `fact <cmp> value`.
    #[must_use]
    pub const fn new(fact: u8, cmp: IntCmp, value: i32) -> Self {
        Self { fact, cmp, value }
    }

    /// Whether `state` satisfies this condition. An out-of-range fact reads as `0`.
    #[must_use]
    pub const fn is_satisfied_by(&self, state: &WorldState) -> bool {
        self.cmp.test(state.get_int(self.fact as usize), self.value)
    }
}

/// How an action changes one integer fact.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum IntEffectKind {
    /// Assign the given value.
    Set(i32),
    /// Add the given delta (saturating at the `i32` bounds, never wrapping).
    Add(i32),
}

/// One integer-fact effect of an action.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct IntEffect {
    /// The integer fact index (`0..MAX_INT_FACTS`).
    pub fact: u8,
    /// How the fact changes.
    pub kind: IntEffectKind,
}

impl IntEffect {
    /// An effect assigning `fact = value`.
    #[must_use]
    pub const fn set(fact: u8, value: i32) -> Self {
        Self {
            fact,
            kind: IntEffectKind::Set(value),
        }
    }

    /// An effect adding `delta` to `fact` (saturating).
    #[must_use]
    pub const fn add(fact: u8, delta: i32) -> Self {
        Self {
            fact,
            kind: IntEffectKind::Add(delta),
        }
    }

    /// Applies this effect. An out-of-range fact is ignored.
    const fn apply_to(&self, state: &mut WorldState) {
        let fact = self.fact as usize;
        if fact >= MAX_INT_FACTS {
            return;
        }
        state.ints[fact] = match self.kind {
            IntEffectKind::Set(value) => value,
            IntEffectKind::Add(delta) => state.ints[fact].saturating_add(delta),
        };
    }
}

/// A partial world state to reach: boolean requirements, integer conditions, and optional
/// per-fact weights for the heuristic.
///
/// Weights only shape the heuristic (how much each unsatisfied fact counts); they never
/// change what counts as success. Every weight defaults to `1.0`, under which the heuristic
/// counts unsatisfied goal facts. Non-positive or non-finite weights are ignored by the
/// builders, so a goal is always valid.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Goal {
    /// Boolean requirements.
    bool_conds: BoolConditions,
    /// Integer conditions.
    int_conds: Vec<IntCondition>,
    /// Heuristic weights of the integer conditions, parallel to `int_conds`.
    int_weights: Vec<f32>,
    /// Heuristic weights of boolean facts; absent facts weigh `1.0`.
    bool_weights: BTreeMap<u8, f32>,
}

impl Goal {
    /// An empty goal, satisfied by every state.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A goal from boolean requirements alone.
    #[must_use]
    pub fn from_bool(conds: BoolConditions) -> Self {
        Self {
            bool_conds: conds,
            ..Self::default()
        }
    }

    /// Requires boolean `fact` to equal `value`. Out-of-range facts are ignored.
    #[must_use]
    pub fn require(mut self, fact: usize, value: bool) -> Self {
        self.bool_conds = self.bool_conds.with(fact, value);
        self
    }

    /// Requires the integer condition to hold, with the default heuristic weight `1.0`.
    /// Conditions on out-of-range facts are ignored.
    #[must_use]
    pub fn require_int(self, cond: IntCondition) -> Self {
        self.require_int_weighted(cond, 1.0)
    }

    /// Requires the integer condition to hold, weighing its contribution to the heuristic by
    /// `weight`. Out-of-range facts are ignored; a non-positive or non-finite weight reads
    /// as `1.0`.
    #[must_use]
    pub fn require_int_weighted(mut self, cond: IntCondition, weight: f32) -> Self {
        if (cond.fact as usize) < MAX_INT_FACTS {
            self.int_conds.push(cond);
            self.int_weights.push(sanitize_weight(weight));
        }
        self
    }

    /// Sets the heuristic weight of boolean `fact`. Out-of-range facts and non-positive or
    /// non-finite weights are ignored.
    #[must_use]
    pub fn with_weight(mut self, fact: usize, weight: f32) -> Self {
        // Fact indices fit in u8: MAX_BOOL_FACTS is 128.
        let Ok(fact_key) = u8::try_from(fact) else {
            return self;
        };
        if fact < MAX_BOOL_FACTS && weight.is_finite() && weight > 0.0 {
            self.bool_weights.insert(fact_key, weight);
        }
        self
    }

    /// The boolean requirements.
    #[must_use]
    pub const fn bool_conditions(&self) -> BoolConditions {
        self.bool_conds
    }

    /// The integer conditions, in insertion order.
    #[must_use]
    pub fn int_conditions(&self) -> &[IntCondition] {
        &self.int_conds
    }

    /// The heuristic weight of boolean `fact` (`1.0` unless set with
    /// [`Goal::with_weight`]).
    #[must_use]
    pub fn bool_weight(&self, fact: usize) -> f32 {
        u8::try_from(fact)
            .ok()
            .and_then(|fact| self.bool_weights.get(&fact))
            .copied()
            .unwrap_or(1.0)
    }

    /// Whether the goal imposes no condition at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bool_conds.is_empty() && self.int_conds.is_empty()
    }

    /// Whether `state` satisfies every boolean requirement and integer condition.
    #[must_use]
    pub fn is_satisfied_by(&self, state: &WorldState) -> bool {
        self.bool_conds.is_satisfied_by(state)
            && self.int_conds.iter().all(|cond| cond.is_satisfied_by(state))
    }

    /// How many goal facts (boolean requirements plus integer conditions) `state` leaves
    /// unsatisfied.
    #[must_use]
    pub fn unsatisfied_count(&self, state: &WorldState) -> usize {
        self.bool_conds.unsatisfied_count(state)
            + self
                .int_conds
                .iter()
                .filter(|cond| !cond.is_satisfied_by(state))
                .count()
    }

    /// The weighted sum of the unsatisfied goal facts in `state` (each fact counted with
    /// its heuristic weight).
    #[must_use]
    pub fn weighted_unsatisfied(&self, state: &WorldState) -> f32 {
        let mut total = 0.0f32;
        if self.bool_weights.is_empty() {
            // Every boolean weight is 1.0: the sum is the count (exact in f32).
            total = self.bool_conds.unsatisfied_count(state) as f32;
        } else {
            // Visit only the unsatisfied facts, in ascending fact order (the same summation
            // order as a full scan, so the result is bit-identical).
            for word in 0..2 {
                let mut unsatisfied =
                    (state.bits[word] ^ self.bool_conds.values[word]) & self.bool_conds.mask[word];
                while unsatisfied != 0 {
                    let fact = word * 64 + unsatisfied.trailing_zeros() as usize;
                    total += self.bool_weight(fact);
                    unsatisfied &= unsatisfied - 1;
                }
            }
        }
        for (cond, weight) in self.int_conds.iter().zip(&self.int_weights) {
            if !cond.is_satisfied_by(state) {
                total += *weight;
            }
        }
        total
    }
}

/// Reads a heuristic weight as a well-formed positive value; anything else reads as `1.0`.
fn sanitize_weight(weight: f32) -> f32 {
    if weight.is_finite() && weight > 0.0 {
        weight
    } else {
        1.0
    }
}

/// A state-dependent action cost: called with the state the action would run in and returns
/// the cost of running it there. The callback must be a pure, deterministic function of the
/// state (the plan cache relies on it), must return a finite, non-negative cost, and must not
/// panic; a bad return value fails the whole planning request with
/// [`PlanError::InvalidAction`].
pub type DynamicCost = Arc<dyn Fn(&WorldState) -> f32 + Send + Sync>;

/// One planner action: the preconditions that must hold for it to run, the effects it has
/// when it runs, and what it costs.
///
/// Build with [`Action::new`] and the chainable `requires` / `sets` / `requires_int` /
/// `sets_int` / `adds_int` methods, or fill the public fields directly. All fields are
/// sanitized again when the action is handed to a [`Planner`]: an empty [`name`](Action::name),
/// a negative or non-finite [`cost`](Action::cost), or an integer fact at or beyond
/// [`MAX_INT_FACTS`] makes every planning request on that planner return
/// [`PlanError::InvalidAction`].
#[derive(Clone)]
pub struct Action {
    /// Action name (the identity a game maps back to behavior; must be non-empty).
    pub name: String,
    /// Boolean preconditions.
    pub preconditions: BoolConditions,
    /// Boolean effects, applied as assignments.
    pub effects: BoolConditions,
    /// Integer preconditions, all of which must hold.
    pub int_preconditions: Vec<IntCondition>,
    /// Integer effects, applied in order after the boolean effects.
    pub int_effects: Vec<IntEffect>,
    /// The static cost of running the action (finite and non-negative). Used directly when
    /// [`dynamic_cost`](Action::dynamic_cost) is `None`, and ignored otherwise.
    pub cost: f32,
    /// An optional state-dependent cost replacing [`cost`](Action::cost).
    pub dynamic_cost: Option<DynamicCost>,
}

impl std::fmt::Debug for Action {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Action")
            .field("name", &self.name)
            .field("preconditions", &self.preconditions)
            .field("effects", &self.effects)
            .field("int_preconditions", &self.int_preconditions)
            .field("int_effects", &self.int_effects)
            .field("cost", &self.cost)
            .field("has_dynamic_cost", &self.dynamic_cost.is_some())
            .finish()
    }
}

impl Action {
    /// An action with no preconditions, no effects, and the given static cost.
    #[must_use]
    pub fn new(name: impl Into<String>, cost: f32) -> Self {
        Self {
            name: name.into(),
            preconditions: BoolConditions::new(),
            effects: BoolConditions::new(),
            int_preconditions: Vec::new(),
            int_effects: Vec::new(),
            cost,
            dynamic_cost: None,
        }
    }

    /// Adds the boolean precondition `fact == value`. Out-of-range facts are ignored.
    #[must_use]
    pub fn requires(mut self, fact: usize, value: bool) -> Self {
        self.preconditions = self.preconditions.with(fact, value);
        self
    }

    /// Adds the boolean effect `fact = value`. Out-of-range facts are ignored.
    #[must_use]
    pub fn sets(mut self, fact: usize, value: bool) -> Self {
        self.effects = self.effects.with(fact, value);
        self
    }

    /// Adds an integer precondition.
    #[must_use]
    pub fn requires_int(mut self, cond: IntCondition) -> Self {
        self.int_preconditions.push(cond);
        self
    }

    /// Adds the integer effect `fact = value`.
    #[must_use]
    pub fn sets_int(mut self, fact: u8, value: i32) -> Self {
        self.int_effects.push(IntEffect::set(fact, value));
        self
    }

    /// Adds the integer effect `fact += delta` (saturating).
    #[must_use]
    pub fn adds_int(mut self, fact: u8, delta: i32) -> Self {
        self.int_effects.push(IntEffect::add(fact, delta));
        self
    }

    /// Replaces the static cost with a state-dependent cost callback (see [`DynamicCost`]
    /// for the contract the callback must keep).
    #[must_use]
    pub fn with_dynamic_cost(mut self, cost: impl Fn(&WorldState) -> f32 + Send + Sync + 'static) -> Self {
        self.dynamic_cost = Some(Arc::new(cost));
        self
    }

    /// Whether every precondition holds in `state`.
    #[must_use]
    pub fn is_applicable(&self, state: &WorldState) -> bool {
        self.preconditions.is_satisfied_by(state)
            && self
                .int_preconditions
                .iter()
                .all(|cond| cond.is_satisfied_by(state))
    }

    /// The state after running this action in `state`: the boolean effects as assignments,
    /// then the integer effects in declaration order (later effects on the same fact see
    /// earlier ones). Does not check preconditions; see [`Action::is_applicable`].
    #[must_use]
    pub fn apply(&self, state: &WorldState) -> WorldState {
        let mut next = *state;
        self.effects.apply_to(&mut next);
        for effect in &self.int_effects {
            effect.apply_to(&mut next);
        }
        next
    }

    /// The cost of running this action in `state`.
    ///
    /// # Errors
    ///
    /// Returns [`PlanError::InvalidAction`] when the static cost — or the value the dynamic
    /// cost callback returns for this state — is negative or non-finite.
    pub fn cost_for(&self, state: &WorldState) -> Result<f32, PlanError> {
        let cost = match &self.dynamic_cost {
            Some(dynamic) => dynamic(state),
            None => self.cost,
        };
        if cost.is_finite() && cost >= 0.0 {
            Ok(cost)
        } else {
            Err(PlanError::InvalidAction)
        }
    }

    /// Whether this action is well formed (see the type-level docs for the rules). The
    /// dynamic cost callback cannot be checked here; it is checked at every call site during
    /// a search.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        !self.name.is_empty()
            && self.cost.is_finite()
            && self.cost >= 0.0
            && self
                .int_preconditions
                .iter()
                .all(|cond| (cond.fact as usize) < MAX_INT_FACTS)
            && self
                .int_effects
                .iter()
                .all(|effect| (effect.fact as usize) < MAX_INT_FACTS)
    }

    /// An upper bound on the heuristic weight of `goal` facts this one action could satisfy
    /// in a single step: the boolean goal facts it sets to their wanted value, plus every
    /// integer goal condition whose fact it touches (whether the touch could satisfy the
    /// condition depends on the state, so counting it keeps the bound an overestimate,
    /// which is what admissibility needs).
    fn max_goal_weight_satisfiable(&self, goal: &Goal) -> f32 {
        let mut total = 0.0f32;
        for fact in 0..MAX_BOOL_FACTS {
            let hits = goal
                .bool_conds
                .get(fact)
                .is_some_and(|wanted| self.effects.get(fact) == Some(wanted));
            if hits {
                total += goal.bool_weight(fact);
            }
        }
        for (cond, weight) in goal.int_conds.iter().zip(&goal.int_weights) {
            if self.int_effects.iter().any(|effect| effect.fact == cond.fact) {
                total += *weight;
            }
        }
        total
    }
}

/// Knobs for one planning request.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlanOptions {
    /// The maximum number of search nodes to expand (the start state counts as the first).
    /// Reaching it with unexplored frontier left returns [`PlanError::LimitReached`]. `0`
    /// allows only the already-satisfied empty plan.
    pub max_nodes: usize,
    /// The maximum number of actions in a plan. A frontier node at this depth is not
    /// expanded; if such a node still had an applicable, state-changing action when the
    /// frontier empties, the result is [`PlanError::LimitReached`] rather than
    /// [`PlanError::NoPlan`].
    pub max_depth: usize,
    /// Multiplier on the admissible heuristic. `1.0` (the default) keeps the heuristic
    /// admissible, so the returned plan has minimum cost; `0.0` degrades the search to
    /// Dijkstra's algorithm (still optimal, usually slower); values above `1.0` give
    /// weighted A\* — typically faster, but the plan is only bounded-suboptimal (its cost is
    /// at most roughly this factor times the optimum). A negative or non-finite value reads
    /// as `1.0`.
    pub heuristic_weight: f32,
    /// Whether to consult and populate the planner's plan cache for this request.
    pub use_cache: bool,
}

impl Default for PlanOptions {
    /// 10 000 expanded nodes, depth 64, the admissible heuristic, cache on.
    fn default() -> Self {
        Self {
            max_nodes: 10_000,
            max_depth: 64,
            heuristic_weight: 1.0,
            use_cache: true,
        }
    }
}

impl PlanOptions {
    /// The heuristic weight with degenerate values read as `1.0`.
    fn effective_heuristic_weight(&self) -> f32 {
        if self.heuristic_weight.is_finite() && self.heuristic_weight >= 0.0 {
            self.heuristic_weight
        } else {
            1.0
        }
    }
}

/// One step of a [`Plan`]: which action to run, and what the planner charged for it in the
/// state it runs in.
#[derive(Clone, Debug, PartialEq)]
pub struct PlanStep {
    /// The index of the action in the planner's action list.
    pub action_index: usize,
    /// The action's name (copied, so a plan stays readable after the planner is rebuilt).
    pub name: String,
    /// The cost charged for this step (the dynamic cost where one applies).
    pub cost: f32,
}

/// The verdict of re-checking a plan (or its remaining suffix) against a world state: see
/// [`Plan::validate_remaining`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlanValidation {
    /// The first step (counted from the start of the whole plan) whose preconditions no
    /// longer hold when the preceding steps are simulated from the given state, or `None`
    /// when every checked step is still applicable.
    pub first_invalid_step: Option<usize>,
    /// Whether simulating the checked steps from the given state ends in a state satisfying
    /// the plan's goal. Always `false` when a step is invalid.
    pub goal_reached: bool,
}

impl PlanValidation {
    /// Whether the checked steps are all still applicable and still reach the goal.
    #[must_use]
    pub const fn is_valid(&self) -> bool {
        self.first_invalid_step.is_none() && self.goal_reached
    }
}

/// A found plan: an ordered list of steps, its total cost, and a snapshot of the step
/// actions and the goal it was planned for, so it can be re-validated (and its validity
/// explained) without the planner that produced it.
#[derive(Clone, Debug)]
pub struct Plan {
    /// The steps, in execution order.
    steps: Vec<PlanStep>,
    /// Snapshots of the step actions, parallel to `steps`.
    actions: Vec<Action>,
    /// The sum of the step costs.
    total_cost: f32,
    /// The goal this plan was planned against.
    goal: Goal,
    /// Search nodes expanded to find this plan (`0` for an already-satisfied goal).
    nodes_expanded: usize,
    /// Whether this plan came out of the planner's cache.
    cache_hit: bool,
}

impl Plan {
    /// The steps of the plan, in the order they should run.
    #[must_use]
    pub fn steps(&self) -> &[PlanStep] {
        &self.steps
    }

    /// The action names of the steps, in execution order.
    pub fn action_names(&self) -> impl Iterator<Item = &str> {
        self.steps.iter().map(|step| step.name.as_str())
    }

    /// The summed cost of the steps (the value the search minimized).
    #[must_use]
    pub const fn total_cost(&self) -> f32 {
        self.total_cost
    }

    /// The goal this plan reaches.
    #[must_use]
    pub const fn goal(&self) -> &Goal {
        &self.goal
    }

    /// How many steps the plan has.
    #[must_use]
    pub fn len(&self) -> usize {
        self.steps.len()
    }

    /// Whether the plan has no steps (the start state already satisfied the goal).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// How many search nodes were expanded to find this plan.
    #[must_use]
    pub const fn nodes_expanded(&self) -> usize {
        self.nodes_expanded
    }

    /// Whether this plan was served from the planner's cache instead of a fresh search.
    #[must_use]
    pub const fn cache_hit(&self) -> bool {
        self.cache_hit
    }

    /// Whether running the whole plan from `state` still works: every step's preconditions
    /// hold in sequence and the final state satisfies the plan's goal. This is the
    /// plan-invalidation check to run before (or while) executing a plan; on `false`, call
    /// [`Planner::replan`].
    #[must_use]
    pub fn validate(&self, state: &WorldState) -> bool {
        self.validate_remaining(state, 0).is_valid()
    }

    /// Re-checks the steps from `next_step` onward against `state` (the world as it is now,
    /// part-way through executing the plan), simulating each step's effects in order.
    /// `next_step` past the end checks no steps, so the verdict then rests on whether
    /// `state` already satisfies the goal.
    #[must_use]
    pub fn validate_remaining(&self, state: &WorldState, next_step: usize) -> PlanValidation {
        let from = next_step.min(self.steps.len());
        let mut current = *state;
        for (offset, action) in self.actions[from..].iter().enumerate() {
            if !action.is_applicable(&current) {
                return PlanValidation {
                    first_invalid_step: Some(from + offset),
                    goal_reached: false,
                };
            }
            current = action.apply(&current);
        }
        PlanValidation {
            first_invalid_step: None,
            goal_reached: self.goal.is_satisfied_by(&current),
        }
    }

    /// The state reached by simulating the steps from `next_step` onward, or `None` at the
    /// first step whose preconditions do not hold.
    #[must_use]
    pub fn simulate(&self, state: &WorldState, next_step: usize) -> Option<WorldState> {
        let from = next_step.min(self.steps.len());
        let mut current = *state;
        for action in &self.actions[from..] {
            if !action.is_applicable(&current) {
                return None;
            }
            current = action.apply(&current);
        }
        Some(current)
    }

    /// The sub-plan from `next_step` onward, with its cost re-summed from the kept steps.
    /// `next_step` past the end yields an empty plan for the same goal.
    #[must_use]
    pub fn suffix(&self, next_step: usize) -> Plan {
        let from = next_step.min(self.steps.len());
        Plan {
            steps: self.steps[from..].to_vec(),
            actions: self.actions[from..].to_vec(),
            total_cost: self.steps[from..].iter().map(|step| step.cost).sum(),
            goal: self.goal.clone(),
            nodes_expanded: 0,
            cache_hit: false,
        }
    }
}

/// One node of the A\* search graph, stored in an arena.
struct SearchNode {
    /// The state this node represents.
    state: WorldState,
    /// The cost of the cheapest known path from the start to `state`.
    g: f32,
    /// The number of actions on that path.
    depth: usize,
    /// The previous node on that path (arena index), `None` at the start.
    parent: Option<usize>,
    /// The planner action that produced this node (`None` at the start).
    action_index: Option<usize>,
    /// The cost charged for that action.
    step_cost: f32,
}

/// One entry of the A\* open list. Ordering is fully deterministic: ascending
/// `f = g + h`, then ascending `h` (prefer the node estimated closer to the goal), then the
/// insertion sequence number (first generated, first expanded) — so equal-cost alternatives
/// always resolve in action-declaration order and a repeated search yields the same plan.
///
/// `BinaryHeap` is a max-heap, so `Ord` is written reversed.
struct OpenEntry {
    /// Estimated total cost through this node.
    f: f32,
    /// Heuristic estimate from this node to the goal.
    h: f32,
    /// Cost from the start to this node, for stale-entry detection.
    g: f32,
    /// Monotonic insertion counter.
    seq: u64,
    /// Arena index of the node.
    node: usize,
}

impl PartialEq for OpenEntry {
    fn eq(&self, other: &Self) -> bool {
        self.f.to_bits() == other.f.to_bits()
            && self.h.to_bits() == other.h.to_bits()
            && self.g.to_bits() == other.g.to_bits()
            && self.seq == other.seq
            && self.node == other.node
    }
}

impl Eq for OpenEntry {}

impl Ord for OpenEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reversed for the max-heap, and total (no reliance on float partial order).
        other
            .f
            .total_cmp(&self.f)
            .then_with(|| other.h.total_cmp(&self.h))
            .then_with(|| other.seq.cmp(&self.seq))
            .then_with(|| other.node.cmp(&self.node))
    }
}

impl PartialOrd for OpenEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// The cache key of a planning request: everything a cached answer may depend on.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct CacheKey {
    /// The start state.
    state: WorldState,
    /// The goal's boolean mask and wanted values.
    goal_bool: ([u64; 2], [u64; 2]),
    /// `(fact, weight bits)` for each weighted boolean fact, in fact order.
    goal_bool_weights: Vec<(u8, u32)>,
    /// `(fact, cmp code, value, weight bits)` for each integer condition, in order.
    goal_ints: Vec<(u8, u8, i32, u32)>,
    /// The request's node limit (a smaller budget must not be answered from a wider search
    /// being assumed, and vice versa, the key records exactly what was asked).
    max_nodes: usize,
    /// The request's depth limit.
    max_depth: usize,
    /// The effective heuristic weight, as bits (a weighted-A\* answer must not serve an
    /// admissible request).
    heuristic_weight_bits: u32,
}

impl CacheKey {
    /// Builds the key of a request. Only called once the request is known to need a search.
    fn new(state: &WorldState, goal: &Goal, options: &PlanOptions) -> Self {
        Self {
            state: *state,
            goal_bool: (goal.bool_conds.mask(), goal.bool_conds.values()),
            goal_bool_weights: goal
                .bool_weights
                .iter()
                .map(|(fact, weight)| (*fact, weight.to_bits()))
                .collect(),
            goal_ints: goal
                .int_conds
                .iter()
                .zip(&goal.int_weights)
                .map(|(cond, weight)| (cond.fact, cond.cmp.code(), cond.value, weight.to_bits()))
                .collect(),
            max_nodes: options.max_nodes,
            max_depth: options.max_depth,
            heuristic_weight_bits: options.effective_heuristic_weight().to_bits(),
        }
    }
}

/// The default number of plans kept in a [`Planner`]'s cache.
const DEFAULT_CACHE_CAPACITY: usize = 256;

/// A GOAP planner over a fixed action set.
///
/// Create with [`Planner::new`] (or [`Planner::try_new`] to reject a malformed action set up
/// front) and query with [`Planner::plan`]. The action list order is significant: it is the
/// deterministic tie-break order for equal-cost plans. Successful searches are cached by
/// (state, goal, options); the cache holds up to a fixed capacity of plans and is cleared
/// whole when it fills (deterministic, allocation-bounded) or when an action is added.
pub struct Planner {
    /// The actions, in tie-break order.
    actions: Vec<Action>,
    /// Whether every action passed [`Action::is_valid`] when it was added.
    valid: bool,
    /// Cached successful plans. A `Mutex` rather than a `RefCell` so a planner is `Sync`
    /// and can live in an ECS component or resource.
    cache: Mutex<BTreeMap<CacheKey, Arc<Plan>>>,
    /// How many plans to keep before clearing the cache.
    cache_capacity: usize,
}

impl std::fmt::Debug for Planner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Planner")
            .field("actions", &self.actions)
            .field("valid", &self.valid)
            .field("cached_plans", &self.cache_len())
            .finish()
    }
}

impl Planner {
    /// A planner over `actions`, in the given order (the tie-break order). A malformed
    /// action does not panic here; every [`Planner::plan`] call on this planner returns
    /// [`PlanError::InvalidAction`] instead. Use [`Planner::try_new`] to fail at build time.
    #[must_use]
    pub fn new(actions: Vec<Action>) -> Self {
        let valid = actions.iter().all(Action::is_valid);
        Self {
            actions,
            valid,
            cache: Mutex::new(BTreeMap::new()),
            cache_capacity: DEFAULT_CACHE_CAPACITY,
        }
    }

    /// A planner over `actions`.
    ///
    /// # Errors
    ///
    /// Returns [`PlanError::InvalidAction`] when any action is malformed (see
    /// [`Action::is_valid`]).
    pub fn try_new(actions: Vec<Action>) -> Result<Self, PlanError> {
        let planner = Self::new(actions);
        if planner.valid {
            Ok(planner)
        } else {
            Err(PlanError::InvalidAction)
        }
    }

    /// The planner's actions, in tie-break order.
    #[must_use]
    pub fn actions(&self) -> &[Action] {
        &self.actions
    }

    /// Whether the action set is well formed.
    #[must_use]
    pub const fn is_valid(&self) -> bool {
        self.valid
    }

    /// Appends an action (last in tie-break order). Clears the plan cache, since cached
    /// plans may no longer be optimal, and re-checks validity.
    pub fn add_action(&mut self, action: Action) {
        self.valid = self.valid && action.is_valid();
        self.actions.push(action);
        self.cache_guard().clear();
    }

    /// Sets how many plans the cache keeps before clearing itself. `0` disables caching.
    pub fn set_cache_capacity(&mut self, capacity: usize) {
        self.cache_capacity = capacity;
        if capacity == 0 {
            self.cache_guard().clear();
        }
    }

    /// The plan cache. A poisoned lock (a panic while another thread held it) still holds a
    /// consistent map: every mutation is a single insert or clear.
    fn cache_guard(&self) -> MutexGuard<'_, BTreeMap<CacheKey, Arc<Plan>>> {
        self.cache.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// How many plans are currently cached.
    #[must_use]
    pub fn cache_len(&self) -> usize {
        self.cache_guard().len()
    }

    /// How many plans are cached, or `None` when another thread holds the cache lock.
    #[cfg(test)]
    fn cache_try_len(&self) -> Option<usize> {
        match self.cache.try_lock() {
            Ok(guard) => Some(guard.len()),
            Err(std::sync::TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner().len()),
            Err(std::sync::TryLockError::WouldBlock) => None,
        }
    }

    /// Drops every cached plan.
    pub fn clear_cache(&self) {
        self.cache_guard().clear();
    }

    /// The admissible heuristic scale for `goal`: the cheapest cost one action can have
    /// (`0.0` when any action has a dynamic cost, whose floor cannot be known without
    /// calling it, or when there are no actions) and the most goal weight one action could
    /// satisfy at once (`0.0` when no action touches the goal).
    fn heuristic_scale(&self, goal: &Goal) -> (f32, f32) {
        let mut min_cost = f32::INFINITY;
        let mut max_weight = 0.0f32;
        let mut any_dynamic = false;
        for action in &self.actions {
            if action.dynamic_cost.is_some() {
                any_dynamic = true;
            } else if action.cost < min_cost {
                min_cost = action.cost;
            }
            let weight = action.max_goal_weight_satisfiable(goal);
            if weight > max_weight {
                max_weight = weight;
            }
        }
        let min_cost = if any_dynamic || !min_cost.is_finite() {
            0.0
        } else {
            min_cost
        };
        (min_cost, max_weight)
    }

    /// Finds a minimum-cost sequence of actions taking `start` to a state satisfying `goal`,
    /// by a forward A\* search over world states.
    ///
    /// The heuristic estimates the remaining cost from the weighted count of unsatisfied
    /// goal facts `W` as `ceil(W / Wmax) * c_min`, where `Wmax` is the most goal weight any
    /// single action could satisfy at once and `c_min` the cheapest action cost: at least
    /// that many actions are still needed, each costing at least `c_min`. With unit weights,
    /// one-fact-at-a-time effects and unit minimum cost this is exactly the classic
    /// unsatisfied-fact count; the scaling keeps it admissible (never an overestimate) when
    /// one action satisfies several goal facts or costs are fractional, so with
    /// [`heuristic_weight`](PlanOptions::heuristic_weight) at most `1.0` the returned plan
    /// is cost-optimal. Actions with a dynamic cost force `c_min` to `0` (a zero heuristic),
    /// which keeps the search optimal for arbitrary pure cost callbacks.
    ///
    /// Actions are considered in declaration order and ties break deterministically (see the
    /// open-list ordering), so the same request always returns the same plan. When two
    /// equal-cost paths reach the same state, the one with fewer steps is kept (it can only
    /// fit a depth limit better); between equal-cost, equal-length paths the first found in
    /// declaration order wins. The depth limit is exact: if any plan of at most
    /// [`max_depth`](PlanOptions::max_depth) steps exists, the cheapest such plan is found
    /// (within the node budget). An action that
    /// leaves the state unchanged is never useful and is skipped. A start state that already
    /// satisfies the goal returns an empty plan, even with a zero node budget.
    ///
    /// # Errors
    ///
    /// - [`PlanError::InvalidAction`] when the action set is malformed, or a dynamic cost
    ///   callback returns a negative or non-finite cost during the search.
    /// - [`PlanError::LimitReached`] when the node budget runs out, or the frontier empties
    ///   only because the depth limit cut off expandable nodes.
    /// - [`PlanError::NoPlan`] when the reachable state space (within the depth limit)
    ///   contains no goal state.
    pub fn plan(&self, start: &WorldState, goal: &Goal, options: &PlanOptions) -> Result<Plan, PlanError> {
        let _span = tracing::debug_span!(target: "regia_ai", "goap_plan").entered();
        if !self.valid {
            return Err(PlanError::InvalidAction);
        }
        if goal.is_satisfied_by(start) {
            return Ok(Plan {
                steps: Vec::new(),
                actions: Vec::new(),
                total_cost: 0.0,
                goal: goal.clone(),
                nodes_expanded: 0,
                cache_hit: false,
            });
        }

        let cache_key = if options.use_cache && self.cache_capacity > 0 {
            Some(CacheKey::new(start, goal, options))
        } else {
            None
        };
        let cached = if let Some(key) = cache_key.as_ref() {
            let guard = self.cache_guard();
            guard.get(key).cloned()
        } else {
            None
        };
        if let Some(plan) = cached {
            // The arc was cloned under the lock. The plan value is cloned only after the guard
            // is gone, so a cache hit does not hold the mutex across that copy.
            #[cfg(test)]
            cache_hit_hook();
            let mut plan = (*plan).clone();
            plan.cache_hit = true;
            return Ok(plan);
        }

        let weight = options.effective_heuristic_weight();
        let (min_cost, max_weight) = self.heuristic_scale(goal);
        let heuristic = |state: &WorldState| -> f32 {
            scaled_heuristic(goal.weighted_unsatisfied(state), max_weight, min_cost, weight)
        };

        let mut nodes: Vec<SearchNode> = Vec::new();
        // Per state, the Pareto front of known paths by (cost, depth) as (g, depth, node).
        // A path is pruned only when another is no costlier *and* no deeper, so a cheap path
        // that hits the depth limit never hides a costlier, shorter one that fits under it.
        // Without a binding depth limit the front is almost always one entry.
        let mut fronts: BTreeMap<WorldState, PathFront> = BTreeMap::new();
        let mut open: BinaryHeap<OpenEntry> = BinaryHeap::new();
        let mut seq: u64 = 0;
        let start_h = heuristic(start);
        nodes.push(SearchNode {
            state: *start,
            g: 0.0,
            depth: 0,
            parent: None,
            action_index: None,
            step_cost: 0.0,
        });
        fronts.insert(
            *start,
            PathFront::new(PathKey {
                g: 0.0,
                depth: 0,
                node: 0,
            }),
        );
        open.push(OpenEntry {
            f: start_h,
            h: start_h,
            g: 0.0,
            seq,
            node: 0,
        });
        seq += 1;

        let mut expanded: usize = 0;
        let mut depth_limited = false;

        while let Some(entry) = open.pop() {
            // Arena indices are always in range: entries are only pushed with fresh nodes.
            let Some(node) = nodes.get(entry.node) else {
                continue;
            };
            // A stale entry: a path to this state that is no costlier and no deeper was
            // found after it was queued. Equal paths never replace the first, which is what
            // makes the choice among equal-cost, equal-length plans follow
            // action-declaration order.
            let live = fronts
                .get(&node.state)
                .is_some_and(|front| front.holds_node(entry.node));
            if !live {
                continue;
            }
            let state = node.state;
            let g = node.g;
            let depth = node.depth;

            if goal.is_satisfied_by(&state) {
                let plan = reconstruct_plan(&nodes, entry.node, &self.actions, goal, expanded);
                if let Some(key) = cache_key {
                    let stored = Arc::new(plan.clone());
                    let mut cache = self.cache_guard();
                    if cache.len() >= self.cache_capacity {
                        cache.clear();
                    }
                    cache.insert(key, stored);
                }
                return Ok(plan);
            }

            if expanded >= options.max_nodes {
                return Err(PlanError::LimitReached);
            }
            expanded += 1;

            if depth >= options.max_depth {
                // Only an action that would actually change the state makes this frontier
                // extendable; a no-op is never generated at any depth.
                if self
                    .actions
                    .iter()
                    .any(|action| action.is_applicable(&state) && action.apply(&state) != state)
                {
                    depth_limited = true;
                }
                continue;
            }

            for (action_index, action) in self.actions.iter().enumerate() {
                if !action.is_applicable(&state) {
                    continue;
                }
                let step_cost = action.cost_for(&state)?;
                let next = action.apply(&state);
                if next == state {
                    // A no-op step can only add cost and depth; skipping it also keeps
                    // zero-cost self-loops out of the graph.
                    continue;
                }
                let next_g = g + step_cost;
                if !next_g.is_finite() {
                    return Err(PlanError::InvalidAction);
                }
                let next_depth = depth + 1;
                let node_id = nodes.len();
                let key = PathKey {
                    g: next_g,
                    depth: next_depth,
                    node: node_id,
                };
                match fronts.entry(next) {
                    Entry::Vacant(vacant) => {
                        vacant.insert(PathFront::new(key));
                    }
                    Entry::Occupied(mut occupied) => {
                        if !occupied.get_mut().insert(key) {
                            continue;
                        }
                    }
                }
                let h = heuristic(&next);
                nodes.push(SearchNode {
                    state: next,
                    g: next_g,
                    depth: next_depth,
                    parent: Some(entry.node),
                    action_index: Some(action_index),
                    step_cost,
                });
                open.push(OpenEntry {
                    f: next_g + h,
                    h,
                    g: next_g,
                    seq,
                    node: node_id,
                });
                seq += 1;
            }
        }

        if depth_limited {
            Err(PlanError::LimitReached)
        } else {
            Err(PlanError::NoPlan)
        }
    }

    /// Whether `plan`'s steps from `next_step` onward still run from `state` and still reach
    /// the plan's goal, judged with this planner's current action definitions (looked up by
    /// the steps' action indices) rather than the plan's snapshots. A step naming an action
    /// index this planner no longer has is invalid. Use [`Plan::validate_remaining`] to judge
    /// with the definitions the plan was made from.
    #[must_use]
    pub fn validate(&self, plan: &Plan, state: &WorldState, next_step: usize) -> bool {
        self.validate_remaining(plan, state, next_step).is_valid()
    }

    /// Detailed form of [`Planner::validate`].
    #[must_use]
    pub fn validate_remaining(&self, plan: &Plan, state: &WorldState, next_step: usize) -> PlanValidation {
        let from = next_step.min(plan.steps.len());
        let mut current = *state;
        for (offset, step) in plan.steps[from..].iter().enumerate() {
            let Some(action) = self.actions.get(step.action_index) else {
                return PlanValidation {
                    first_invalid_step: Some(from + offset),
                    goal_reached: false,
                };
            };
            if !action.is_applicable(&current) {
                return PlanValidation {
                    first_invalid_step: Some(from + offset),
                    goal_reached: false,
                };
            }
            current = action.apply(&current);
        }
        PlanValidation {
            first_invalid_step: None,
            goal_reached: plan.goal.is_satisfied_by(&current),
        }
    }

    /// The execution-time replanning entry point: if the remaining suffix of `plan` (from
    /// `next_step`) is still valid from the current `state`, returns that suffix unchanged;
    /// otherwise searches for a fresh plan from `state` to `goal`.
    ///
    /// The kept suffix is judged with the plan's own action snapshots and its recorded step
    /// costs, so a suffix is only discarded when a precondition actually stopped holding or
    /// the goal would no longer be reached — not merely because a cheaper route appeared.
    ///
    /// # Errors
    ///
    /// The errors of [`Planner::plan`], when a fresh search is needed.
    pub fn replan(
        &self,
        plan: &Plan,
        state: &WorldState,
        next_step: usize,
        goal: &Goal,
        options: &PlanOptions,
    ) -> Result<Plan, PlanError> {
        if plan.validate_remaining(state, next_step).is_valid() && *plan.goal() == *goal {
            Ok(plan.suffix(next_step))
        } else {
            self.plan(state, goal, options)
        }
    }
}

/// One known path to a search state: its cost, its length and its arena node.
#[derive(Clone, Copy)]
struct PathKey {
    /// Cost from the start.
    g: f32,
    /// Number of actions from the start.
    depth: usize,
    /// Arena index of the node at the end of the path.
    node: usize,
}

impl PathKey {
    /// Whether this path is no costlier and no deeper than `other` (so `other` can never
    /// lead to a better plan, or to one that fits a depth limit this one does not).
    fn dominates(&self, other: &Self) -> bool {
        self.g <= other.g && self.depth <= other.depth
    }
}

/// The Pareto front of the known paths to one state over (cost, depth). The first path is
/// stored inline, so the common single-path state does not allocate.
struct PathFront {
    /// A path on the front.
    head: PathKey,
    /// The rest of the front (empty unless a depth trade-off exists).
    tail: Vec<PathKey>,
}

impl PathFront {
    /// A front holding one path.
    fn new(key: PathKey) -> Self {
        Self {
            head: key,
            tail: Vec::new(),
        }
    }

    /// Whether the path ending at arena `node` is still on the front.
    fn holds_node(&self, node: usize) -> bool {
        self.head.node == node || self.tail.iter().any(|key| key.node == node)
    }

    /// Adds `key` unless a known path dominates it, dropping the paths it dominates.
    /// Returns whether it was added.
    fn insert(&mut self, key: PathKey) -> bool {
        if self.head.dominates(&key) || self.tail.iter().any(|known| known.dominates(&key)) {
            return false;
        }
        self.tail.retain(|known| !key.dominates(known));
        if key.dominates(&self.head) {
            self.head = key;
        } else {
            self.tail.push(key);
        }
        true
    }
}

/// Slack on the `W / Wmax` quotient before rounding it up to a step count. `W` and `Wmax` are
/// f32 sums of up to `MAX_BOOL_FACTS + MAX_INT_FACTS` positive weights, so each carries a
/// relative rounding error far below this; without the slack, a quotient whose exact value
/// is an integer `n` can land a hair above `n` (for example `0.8000001 / 0.2`) and the
/// `ceil` would charge a whole extra step, overestimating the remaining cost. Rounding a
/// genuinely fractional quotient down by this much only weakens the bound, which keeps it
/// admissible.
const HEURISTIC_QUOTIENT_SLACK: f64 = 1e-4;

/// The admissible heuristic: at least `ceil(unsatisfied / max_weight)` more actions are needed
/// (no action satisfies more than `max_weight` of goal weight), each costing at least
/// `min_cost`, scaled by the request's heuristic `weight`. Zero when the goal is met, when
/// `min_cost` or `weight` is zero, and at least one step when no action touches the goal.
fn scaled_heuristic(unsatisfied: f32, max_weight: f32, min_cost: f32, weight: f32) -> f32 {
    if weight == 0.0 || min_cost == 0.0 || unsatisfied <= 0.0 {
        return 0.0;
    }
    let steps = if max_weight > 0.0 {
        let quotient = f64::from(unsatisfied) / f64::from(max_weight);
        (quotient - HEURISTIC_QUOTIENT_SLACK).ceil().max(1.0)
    } else {
        1.0
    };
    // `steps` is at most (128 + 16) / smallest weight; the f32 product stays finite or
    // saturates to infinity, which only orders a node last.
    (steps * f64::from(min_cost) * f64::from(weight)) as f32
}

/// Walks the parent chain of the goal node back to the start and builds the plan.
fn reconstruct_plan(
    nodes: &[SearchNode],
    mut node_id: usize,
    actions: &[Action],
    goal: &Goal,
    nodes_expanded: usize,
) -> Plan {
    let mut steps: Vec<PlanStep> = Vec::new();
    let mut step_actions: Vec<Action> = Vec::new();
    // The node's own g is the summed cost in execution order; re-summing backwards here
    // could give a different last-bit result for fractional costs.
    let total_cost = nodes.get(node_id).map_or(0.0, |node| node.g);
    // The chain is finite and acyclic: every parent was allocated before its child.
    while let Some(node) = nodes.get(node_id) {
        let Some(parent) = node.parent else { break };
        let found = node
            .action_index
            .and_then(|index| actions.get(index).map(|action| (index, action)));
        if let Some((action_index, action)) = found {
            steps.push(PlanStep {
                action_index,
                name: action.name.clone(),
                cost: node.step_cost,
            });
            step_actions.push(action.clone());
        }
        node_id = parent;
    }
    steps.reverse();
    step_actions.reverse();
    Plan {
        steps,
        actions: step_actions,
        total_cost,
        goal: goal.clone(),
        nodes_expanded,
        cache_hit: false,
    }
}

/// A name-to-fact-index table bridging a [`Blackboard`] and a [`WorldState`]: register the
/// blackboard keys the planner cares about once, then snapshot the blackboard into a state
/// to plan from, and optionally write a simulated state back.
///
/// Boolean facts read `BbValue::Bool` keys (anything else reads as `false`, matching
/// [`Blackboard::get_bool`]); integer facts read numeric keys clamped to the `i32` range.
#[derive(Clone, Debug, Default)]
pub struct FactTable {
    /// Boolean fact names; the position in the list is the fact index.
    bool_names: Vec<String>,
    /// Integer fact names; the position in the list is the fact index.
    int_names: Vec<String>,
}

impl FactTable {
    /// An empty table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a boolean fact name and returns its fact index. Registering the same name
    /// twice returns the existing index; returns `None` once [`MAX_BOOL_FACTS`] names are
    /// registered.
    pub fn add_bool(&mut self, name: impl Into<String>) -> Option<usize> {
        let name = name.into();
        if let Some(index) = self.bool_names.iter().position(|existing| *existing == name) {
            return Some(index);
        }
        if self.bool_names.len() >= MAX_BOOL_FACTS {
            return None;
        }
        let index = self.bool_names.len();
        self.bool_names.push(name);
        Some(index)
    }

    /// Registers an integer fact name and returns its fact index. Registering the same name
    /// twice returns the existing index; returns `None` once [`MAX_INT_FACTS`] names are
    /// registered.
    pub fn add_int(&mut self, name: impl Into<String>) -> Option<usize> {
        let name = name.into();
        if let Some(index) = self.int_names.iter().position(|existing| *existing == name) {
            return Some(index);
        }
        if self.int_names.len() >= MAX_INT_FACTS {
            return None;
        }
        let index = self.int_names.len();
        self.int_names.push(name);
        Some(index)
    }

    /// The index of a registered boolean fact name.
    #[must_use]
    pub fn bool_index(&self, name: &str) -> Option<usize> {
        self.bool_names.iter().position(|existing| existing == name)
    }

    /// The index of a registered integer fact name.
    #[must_use]
    pub fn int_index(&self, name: &str) -> Option<usize> {
        self.int_names.iter().position(|existing| existing == name)
    }

    /// The registered boolean fact names, in fact-index order.
    #[must_use]
    pub fn bool_names(&self) -> &[String] {
        &self.bool_names
    }

    /// The registered integer fact names, in fact-index order.
    #[must_use]
    pub fn int_names(&self) -> &[String] {
        &self.int_names
    }

    /// Snapshots the registered keys of `blackboard` into a world state. Unregistered keys
    /// are ignored; missing keys read as the default (`false` / `0`).
    #[must_use]
    pub fn snapshot(&self, blackboard: &Blackboard) -> WorldState {
        let mut state = WorldState::new();
        for (index, name) in self.bool_names.iter().enumerate() {
            let _ = state.set(index, blackboard.get_bool(name));
        }
        for (index, name) in self.int_names.iter().enumerate() {
            let value = blackboard
                .get_int(name)
                .clamp(i64::from(i32::MIN), i64::from(i32::MAX));
            // The clamp above keeps the value inside the i32 range.
            let _ = state.set_int(index, value as i32);
        }
        state
    }

    /// Writes every registered fact of `state` back into `blackboard` (booleans as
    /// `BbValue::Bool`, integers as `BbValue::Int`), overwriting existing values.
    pub fn write_back(&self, state: &WorldState, blackboard: &mut Blackboard) {
        for (index, name) in self.bool_names.iter().enumerate() {
            blackboard
                .values
                .insert(name.clone(), BbValue::Bool(state.get(index)));
        }
        for (index, name) in self.int_names.iter().enumerate() {
            blackboard
                .values
                .insert(name.clone(), BbValue::Int(i64::from(state.get_int(index))));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Facts of the woodcutter domain.
    const HAS_AXE: usize = 0;
    /// Fact: the agent holds wood.
    const HAS_WOOD: usize = 1;
    /// Fact: the agent holds a crafted table.
    const HAS_TABLE: usize = 2;
    /// Fact: an axe is lying around to pick up.
    const AXE_AVAILABLE: usize = 3;

    fn woodcutter() -> Planner {
        Planner::new(vec![
            Action::new("pick_up_axe", 1.0)
                .requires(AXE_AVAILABLE, true)
                .sets(HAS_AXE, true)
                .sets(AXE_AVAILABLE, false),
            Action::new("chop_tree", 2.0)
                .requires(HAS_AXE, true)
                .sets(HAS_WOOD, true),
            Action::new("collect_sticks", 5.0).sets(HAS_WOOD, true),
            Action::new("craft_table", 1.0)
                .requires(HAS_WOOD, true)
                .sets(HAS_TABLE, true),
        ])
    }

    fn names(plan: &Plan) -> Vec<&str> {
        plan.action_names().collect()
    }

    fn no_cache() -> PlanOptions {
        PlanOptions {
            use_cache: false,
            ..PlanOptions::default()
        }
    }

    #[test]
    fn a_cache_hit_does_not_hold_the_mutex_across_the_plan_clone() {
        use std::sync::atomic::{AtomicU8, Ordering};
        static PHASE: AtomicU8 = AtomicU8::new(0);
        PHASE.store(0, Ordering::Relaxed);
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                CACHE_HIT_HOOK.with(|hook| hook.set(|| {}));
                PHASE.store(2, Ordering::Release);
            }
        }
        let _reset = Reset;
        let planner = woodcutter();
        let start = WorldState::new().with(AXE_AVAILABLE, true);
        let goal = Goal::new().require(HAS_TABLE, true);
        let first = planner
            .plan(&start, &goal, &PlanOptions::default())
            .expect("plan");
        assert!(!first.cache_hit());
        assert_eq!(planner.cache_len(), 1);
        CACHE_HIT_HOOK.with(|hook| {
            hook.set(|| {
                PHASE.store(1, Ordering::Release);
                while PHASE.load(Ordering::Acquire) != 2 {
                    std::thread::yield_now();
                }
            });
        });
        let planner = &planner;
        let seen = std::thread::scope(|scope| {
            let waiter = scope.spawn(|| {
                let mut spins = 0u32;
                while PHASE.load(Ordering::Acquire) != 1 && spins < 1_000_000 {
                    spins += 1;
                    std::thread::yield_now();
                }
                let reached = PHASE.load(Ordering::Acquire) == 1;
                let len = planner.cache_try_len();
                PHASE.store(2, Ordering::Release);
                (reached, len)
            });
            let second = planner
                .plan(&start, &goal, &PlanOptions::default())
                .expect("cached");
            assert!(second.cache_hit());
            assert_eq!(names(&second), names(&first));
            waiter.join().expect("waiter")
        });
        assert!(seen.0, "the cache-hit hook did not run");
        assert_eq!(seen.1, Some(1), "the cache mutex was held across the plan clone");
    }

    #[test]
    fn woodcutter_chop_craft_chain() {
        let planner = woodcutter();
        let start = WorldState::new().with(AXE_AVAILABLE, true);
        let goal = Goal::new().require(HAS_TABLE, true);
        let plan = planner.plan(&start, &goal, &no_cache()).expect("plan exists");
        assert_eq!(names(&plan), ["pick_up_axe", "chop_tree", "craft_table"]);
        assert_eq!(plan.total_cost(), 4.0);
        assert_eq!(plan.len(), 3);
        assert!(plan.validate(&start));
        let end = plan.simulate(&start, 0).expect("simulates");
        assert!(end.get(HAS_TABLE) && end.get(HAS_AXE) && !end.get(AXE_AVAILABLE));
    }

    #[test]
    fn already_satisfied_goal_is_an_empty_plan() {
        let planner = woodcutter();
        let start = WorldState::new().with(HAS_TABLE, true);
        let goal = Goal::new().require(HAS_TABLE, true);
        let options = PlanOptions {
            max_nodes: 0,
            ..no_cache()
        };
        let plan = planner.plan(&start, &goal, &options).expect("empty plan");
        assert!(plan.is_empty());
        assert_eq!(plan.total_cost(), 0.0);
        assert_eq!(plan.nodes_expanded(), 0);
        assert!(plan.validate(&start));
    }

    #[test]
    fn empty_goal_is_an_empty_plan_and_empty_planner_has_no_plan() {
        let planner = Planner::new(Vec::new());
        let start = WorldState::new();
        let plan = planner
            .plan(&start, &Goal::new(), &no_cache())
            .expect("empty goal");
        assert!(plan.is_empty());
        let goal = Goal::new().require(0, true);
        assert_eq!(
            planner.plan(&start, &goal, &no_cache()).err(),
            Some(PlanError::NoPlan)
        );
    }

    #[test]
    fn unreachable_goal_returns_no_plan() {
        let planner = woodcutter();
        // Nothing ever sets fact 7.
        let goal = Goal::new().require(7, true);
        assert_eq!(
            planner.plan(&WorldState::new(), &goal, &no_cache()).err(),
            Some(PlanError::NoPlan)
        );
    }

    #[test]
    fn node_limit_returns_limit_reached() {
        let planner = woodcutter();
        let start = WorldState::new().with(AXE_AVAILABLE, true);
        let goal = Goal::new().require(HAS_TABLE, true);
        for max_nodes in [0usize, 1] {
            let options = PlanOptions {
                max_nodes,
                ..no_cache()
            };
            assert_eq!(
                planner.plan(&start, &goal, &options).err(),
                Some(PlanError::LimitReached),
                "max_nodes {max_nodes}"
            );
        }
        // A goal one expansion away fits a one-node budget: the start is expanded, and the
        // goal test runs when the generated node is popped, before any second expansion.
        let single = Planner::new(vec![Action::new("grab", 1.0).sets(0, true)]);
        let options = PlanOptions {
            max_nodes: 1,
            ..no_cache()
        };
        let plan = single
            .plan(&WorldState::new(), &Goal::new().require(0, true), &options)
            .expect("one expansion suffices");
        assert_eq!(names(&plan), ["grab"]);
        assert_eq!(plan.nodes_expanded(), 1);
    }

    #[test]
    fn depth_limit_returns_limit_reached_not_no_plan() {
        let planner = woodcutter();
        let start = WorldState::new().with(AXE_AVAILABLE, true);
        let goal = Goal::new().require(HAS_TABLE, true);
        // The axe route and the stick route both need more than one step once
        // `collect_sticks` + `craft_table` count as two.
        let options = PlanOptions {
            max_depth: 1,
            ..no_cache()
        };
        assert_eq!(
            planner.plan(&start, &goal, &options).err(),
            Some(PlanError::LimitReached)
        );
        let options = PlanOptions {
            max_depth: 2,
            ..no_cache()
        };
        let plan = planner.plan(&start, &goal, &options).expect("two steps suffice");
        assert_eq!(names(&plan), ["collect_sticks", "craft_table"]);
    }

    #[test]
    fn invalid_actions_are_reported() {
        let start = WorldState::new();
        let goal = Goal::new().require(0, true);
        for action in [
            Action::new("", 1.0).sets(0, true),
            Action::new("nan_cost", f32::NAN).sets(0, true),
            Action::new("negative_cost", -1.0).sets(0, true),
            Action::new("infinite_cost", f32::INFINITY).sets(0, true),
            Action::new("bad_int_fact", 1.0).sets_int(99, 1),
            Action::new("bad_int_pre", 1.0).requires_int(IntCondition::new(200, IntCmp::Ge, 1)),
        ] {
            let name = action.name.clone();
            let planner = Planner::new(vec![action]);
            assert!(!planner.is_valid(), "{name}");
            assert_eq!(
                planner.plan(&start, &goal, &no_cache()).err(),
                Some(PlanError::InvalidAction),
                "{name}"
            );
        }
        let bad = vec![Action::new("x", f32::NAN)];
        assert_eq!(Planner::try_new(bad).err(), Some(PlanError::InvalidAction));
        assert!(Planner::try_new(vec![Action::new("x", 1.0)]).is_ok());
    }

    #[test]
    fn dynamic_cost_drives_the_choice() {
        // Fact 9: a shortcut is open.
        let planner = Planner::new(vec![
            Action::new("direct", 0.0)
                .sets(1, true)
                .with_dynamic_cost(|state| if state.get(9) { 1.0 } else { 10.0 }),
            Action::new("step_a", 1.0).sets(0, true),
            Action::new("step_b", 2.0).requires(0, true).sets(1, true),
        ]);
        let goal = Goal::new().require(1, true);
        let closed = planner
            .plan(&WorldState::new(), &goal, &no_cache())
            .expect("plan");
        assert_eq!(names(&closed), ["step_a", "step_b"]);
        assert_eq!(closed.total_cost(), 3.0);
        let open_start = WorldState::new().with(9, true);
        let open = planner.plan(&open_start, &goal, &no_cache()).expect("plan");
        assert_eq!(names(&open), ["direct"]);
        assert_eq!(open.total_cost(), 1.0);
        assert_eq!(open.steps()[0].cost, 1.0);
    }

    #[test]
    fn dynamic_cost_returning_garbage_is_invalid_action() {
        for bad in [f32::NAN, f32::INFINITY, -0.5] {
            let planner = Planner::new(vec![
                Action::new("bad", 0.0)
                    .sets(0, true)
                    .with_dynamic_cost(move |_| bad),
            ]);
            let goal = Goal::new().require(0, true);
            assert_eq!(
                planner.plan(&WorldState::new(), &goal, &no_cache()).err(),
                Some(PlanError::InvalidAction),
                "cost {bad}"
            );
        }
    }

    #[test]
    fn cheaper_longer_plan_beats_expensive_shortcut() {
        let planner = Planner::new(vec![
            Action::new("shortcut", 9.0).sets(2, true),
            Action::new("a", 1.0).sets(0, true),
            Action::new("b", 1.0).requires(0, true).sets(1, true),
            Action::new("c", 1.0).requires(1, true).sets(2, true),
        ]);
        let goal = Goal::new().require(2, true);
        let plan = planner
            .plan(&WorldState::new(), &goal, &no_cache())
            .expect("plan");
        assert_eq!(names(&plan), ["a", "b", "c"]);
        assert_eq!(plan.total_cost(), 3.0);
    }

    #[test]
    fn one_action_satisfying_two_goal_facts_stays_optimal() {
        // The raw unsatisfied count (2) would overestimate the one-step combo; the scaled
        // heuristic must not, or A* could return the two single steps at the same price via
        // a worse order. Make the combo strictly cheapest to pin the behavior down.
        let planner = Planner::new(vec![
            Action::new("only_a", 2.0).sets(0, true),
            Action::new("only_b", 2.0).sets(1, true),
            Action::new("both", 3.0).sets(0, true).sets(1, true),
        ]);
        let goal = Goal::new().require(0, true).require(1, true);
        let plan = planner
            .plan(&WorldState::new(), &goal, &no_cache())
            .expect("plan");
        assert_eq!(names(&plan), ["both"]);
        assert_eq!(plan.total_cost(), 3.0);
    }

    #[test]
    fn negative_preconditions_and_effects_are_honored() {
        // Fact 0: door locked. Fact 1: inside.
        let planner = Planner::new(vec![
            Action::new("unlock", 1.0).requires(0, true).sets(0, false),
            Action::new("enter", 1.0).requires(0, false).sets(1, true),
            Action::new("smash", 5.0).sets(0, false).sets(1, true),
        ]);
        let start = WorldState::new().with(0, true);
        let goal = Goal::new().require(1, true).require(0, false);
        let plan = planner.plan(&start, &goal, &no_cache()).expect("plan");
        assert_eq!(names(&plan), ["unlock", "enter"]);
        assert!(plan.validate(&start));
    }

    #[test]
    fn integer_facts_gate_and_accumulate() {
        // Int fact 0: wood count. Bool 0: has table.
        let wood = IntCondition::new(0, IntCmp::Ge, 2);
        let planner = Planner::new(vec![
            Action::new("gather", 1.0).adds_int(0, 1),
            Action::new("buy_bundle", 3.0).adds_int(0, 2),
            Action::new("craft", 1.0)
                .requires_int(wood)
                .adds_int(0, -2)
                .sets(0, true),
        ]);
        let goal = Goal::new().require(0, true);
        let plan = planner
            .plan(&WorldState::new(), &goal, &no_cache())
            .expect("plan");
        assert_eq!(names(&plan), ["gather", "gather", "craft"]);
        assert_eq!(plan.total_cost(), 3.0);
        let end = plan.simulate(&WorldState::new(), 0).expect("simulates");
        assert_eq!(end.get_int(0), 0);
        // An integer goal works too: three gathers (3.0) beat a bundle plus a gather (4.0).
        let goal = Goal::new().require_int(IntCondition::new(0, IntCmp::Ge, 3));
        let plan = planner
            .plan(&WorldState::new(), &goal, &no_cache())
            .expect("plan");
        assert_eq!(names(&plan), ["gather", "gather", "gather"]);
        assert_eq!(plan.total_cost(), 3.0);
    }

    #[test]
    fn integer_effects_saturate_instead_of_wrapping() {
        let planner = Planner::new(vec![Action::new("fill", 1.0).adds_int(0, i32::MAX)]);
        let start = WorldState::new().with_int(0, 5);
        let goal = Goal::new().require_int(IntCondition::new(0, IntCmp::Eq, i32::MAX));
        let plan = planner.plan(&start, &goal, &no_cache()).expect("plan");
        let end = plan.simulate(&start, 0).expect("simulates");
        assert_eq!(end.get_int(0), i32::MAX);
    }

    #[test]
    fn int_cmp_variants() {
        let state = WorldState::new().with_int(0, 3);
        for (cmp, value, expected) in [
            (IntCmp::Eq, 3, true),
            (IntCmp::Eq, 4, false),
            (IntCmp::Ne, 4, true),
            (IntCmp::Lt, 4, true),
            (IntCmp::Lt, 3, false),
            (IntCmp::Le, 3, true),
            (IntCmp::Gt, 2, true),
            (IntCmp::Gt, 3, false),
            (IntCmp::Ge, 3, true),
        ] {
            assert_eq!(IntCondition::new(0, cmp, value).is_satisfied_by(&state), expected);
        }
    }

    #[test]
    fn out_of_range_facts_never_panic() {
        let mut state = WorldState::new();
        assert!(!state.set(128, true));
        assert!(!state.set(usize::MAX, true));
        assert!(!state.set_int(16, 1));
        assert!(!state.get(999));
        assert_eq!(state.get_int(999), 0);
        assert!(state.set(127, true));
        assert!(state.get(127));
        assert_eq!(state.count_true(), 1);
        // A condition set and a goal built partly out of range just drop those facts.
        let conds = BoolConditions::new().with(500, true).with(3, true);
        assert_eq!(conds.len(), 1);
        assert_eq!(conds.get(500), None);
        let goal = Goal::new().require(500, true).require(3, true);
        assert_eq!(goal.unsatisfied_count(&state), 1);
        let action = Action::new("a", 1.0).requires(500, true).sets(3, true);
        assert!(action.is_valid());
    }

    #[test]
    fn zero_cost_actions_and_cycles_terminate() {
        let planner = Planner::new(vec![
            Action::new("toggle_on", 0.0).sets(0, true),
            Action::new("toggle_off", 0.0).sets(0, false),
            Action::new("finish", 0.0).requires(0, true).sets(1, true),
        ]);
        let goal = Goal::new().require(1, true);
        let plan = planner
            .plan(&WorldState::new(), &goal, &no_cache())
            .expect("plan");
        assert_eq!(names(&plan), ["toggle_on", "finish"]);
        assert_eq!(plan.total_cost(), 0.0);
        // An unreachable goal over the same cycling domain still terminates as NoPlan.
        let goal = Goal::new().require(2, true);
        assert_eq!(
            planner.plan(&WorldState::new(), &goal, &no_cache()).err(),
            Some(PlanError::NoPlan)
        );
    }

    #[test]
    fn no_op_actions_are_never_used() {
        let planner = Planner::new(vec![
            Action::new("wait", 0.0),
            Action::new("act", 1.0).sets(0, true),
            Action::new("act_again", 0.0).sets(0, true),
        ]);
        let goal = Goal::new().require(0, true);
        let plan = planner
            .plan(&WorldState::new(), &goal, &no_cache())
            .expect("plan");
        // `act_again` is free and does the job in one step; `wait` changes nothing.
        assert_eq!(names(&plan), ["act_again"]);
        assert_eq!(plan.total_cost(), 0.0);
    }

    #[test]
    fn validate_remaining_pinpoints_the_broken_step() {
        let planner = woodcutter();
        let start = WorldState::new().with(AXE_AVAILABLE, true);
        let goal = Goal::new().require(HAS_TABLE, true);
        let plan = planner.plan(&start, &goal, &no_cache()).expect("plan");
        // Step 0 ran; then the axe broke before the chop.
        let mid = WorldState::new();
        let verdict = plan.validate_remaining(&mid, 1);
        assert_eq!(verdict.first_invalid_step, Some(1));
        assert!(!verdict.goal_reached);
        assert!(!verdict.is_valid());
        // The planner's own-definition check agrees.
        assert!(!planner.validate(&plan, &mid, 1));
        assert_eq!(
            planner.validate_remaining(&plan, &mid, 1).first_invalid_step,
            Some(1)
        );
        // From the last step with wood in hand, the suffix is fine.
        let has_wood = WorldState::new().with(HAS_WOOD, true);
        assert!(plan.validate_remaining(&has_wood, 2).is_valid());
        assert_eq!(plan.suffix(2).len(), 1);
        assert_eq!(plan.suffix(99).len(), 0);
        assert!(plan.simulate(&mid, 1).is_none());
    }

    #[test]
    fn replanning_after_a_world_change_picks_the_fallback() {
        let planner = woodcutter();
        let start = WorldState::new().with(AXE_AVAILABLE, true);
        let goal = Goal::new().require(HAS_TABLE, true);
        let plan = planner.plan(&start, &goal, &no_cache()).expect("plan");
        assert_eq!(names(&plan), ["pick_up_axe", "chop_tree", "craft_table"]);
        // The axe was picked up (no longer available) and then broke: the chop step's
        // precondition no longer holds and no second axe exists.
        let now = WorldState::new();
        let revised = planner
            .replan(&plan, &now, 1, &goal, &no_cache())
            .expect("replan");
        assert_eq!(names(&revised), ["collect_sticks", "craft_table"]);
        assert_eq!(revised.total_cost(), 6.0);
        assert!(revised.validate(&now));
    }

    #[test]
    fn replan_keeps_a_still_valid_suffix() {
        let planner = woodcutter();
        let start = WorldState::new().with(AXE_AVAILABLE, true);
        let goal = Goal::new().require(HAS_TABLE, true);
        let plan = planner.plan(&start, &goal, &no_cache()).expect("plan");
        let after_pickup = WorldState::new().with(HAS_AXE, true);
        let kept = planner
            .replan(&plan, &after_pickup, 1, &goal, &no_cache())
            .expect("kept");
        assert_eq!(names(&kept), ["chop_tree", "craft_table"]);
        assert_eq!(kept.total_cost(), 3.0);
    }

    #[test]
    fn replan_to_a_different_goal_searches_afresh() {
        let planner = woodcutter();
        let start = WorldState::new().with(AXE_AVAILABLE, true);
        let plan = planner
            .plan(&start, &Goal::new().require(HAS_TABLE, true), &no_cache())
            .expect("plan");
        let new_goal = Goal::new().require(HAS_WOOD, true);
        let revised = planner
            .replan(&plan, &start, 0, &new_goal, &no_cache())
            .expect("replan");
        assert_eq!(names(&revised), ["pick_up_axe", "chop_tree"]);
    }

    /// Two symmetric two-step routes to fact 2, every action at cost 1.
    fn symmetric() -> Planner {
        Planner::new(vec![
            Action::new("left", 1.0).sets(0, true),
            Action::new("right", 1.0).sets(1, true),
            Action::new("finish_left", 1.0).requires(0, true).sets(2, true),
            Action::new("finish_right", 1.0).requires(1, true).sets(2, true),
        ])
    }

    #[test]
    fn identical_costs_give_a_stable_declaration_order() {
        let goal = Goal::new().require(2, true);
        let plan = symmetric()
            .plan(&WorldState::new(), &goal, &no_cache())
            .expect("plan");
        assert_eq!(names(&plan), ["left", "finish_left"]);
        // Declaring the mirror route first flips the winner: the tie-break is the action
        // list order, not names or hash order.
        let flipped = Planner::new(vec![
            Action::new("right", 1.0).sets(1, true),
            Action::new("left", 1.0).sets(0, true),
            Action::new("finish_right", 1.0).requires(1, true).sets(2, true),
            Action::new("finish_left", 1.0).requires(0, true).sets(2, true),
        ]);
        let plan = flipped
            .plan(&WorldState::new(), &goal, &no_cache())
            .expect("plan");
        assert_eq!(names(&plan), ["right", "finish_right"]);
    }

    #[test]
    fn planning_is_deterministic_across_100_repeats() {
        let planner = symmetric();
        let goal = Goal::new().require(2, true);
        let first = planner
            .plan(&WorldState::new(), &goal, &no_cache())
            .expect("plan");
        let expected: Vec<String> = first.action_names().map(String::from).collect();
        for _ in 0..100 {
            let plan = planner
                .plan(&WorldState::new(), &goal, &no_cache())
                .expect("plan");
            let got: Vec<String> = plan.action_names().map(String::from).collect();
            assert_eq!(got, expected);
            assert_eq!(plan.total_cost().to_bits(), first.total_cost().to_bits());
            assert_eq!(plan.nodes_expanded(), first.nodes_expanded());
        }
    }

    #[test]
    fn the_plan_cache_hits_clears_and_can_be_disabled() {
        let mut planner = woodcutter();
        let start = WorldState::new().with(AXE_AVAILABLE, true);
        let goal = Goal::new().require(HAS_TABLE, true);
        let first = planner
            .plan(&start, &goal, &PlanOptions::default())
            .expect("plan");
        assert!(!first.cache_hit());
        assert_eq!(planner.cache_len(), 1);
        let second = planner
            .plan(&start, &goal, &PlanOptions::default())
            .expect("plan");
        assert!(second.cache_hit());
        assert_eq!(names(&second), names(&first));
        assert_eq!(second.total_cost(), first.total_cost());
        // A different goal is a different entry; a no-cache request adds nothing.
        let wood_goal = Goal::new().require(HAS_WOOD, true);
        let _ = planner
            .plan(&start, &wood_goal, &PlanOptions::default())
            .expect("plan");
        assert_eq!(planner.cache_len(), 2);
        planner.clear_cache();
        assert_eq!(planner.cache_len(), 0);
        let _ = planner.plan(&start, &goal, &no_cache()).expect("plan");
        assert_eq!(planner.cache_len(), 0);
        // Adding an action invalidates cached optima, and capacity 0 disables caching.
        let _ = planner
            .plan(&start, &goal, &PlanOptions::default())
            .expect("plan");
        assert_eq!(planner.cache_len(), 1);
        planner.add_action(Action::new("wait", 0.0));
        assert_eq!(planner.cache_len(), 0);
        planner.set_cache_capacity(0);
        let uncached = planner
            .plan(&start, &goal, &PlanOptions::default())
            .expect("plan");
        assert!(!uncached.cache_hit());
        assert_eq!(planner.cache_len(), 0);
    }

    #[test]
    fn degenerate_options_and_goal_weights_fall_back_safely() {
        let planner = woodcutter();
        let start = WorldState::new().with(AXE_AVAILABLE, true);
        let goal = Goal::new().require(HAS_TABLE, true);
        for bad_weight in [f32::NAN, f32::INFINITY, -3.0] {
            let options = PlanOptions {
                heuristic_weight: bad_weight,
                ..no_cache()
            };
            let plan = planner.plan(&start, &goal, &options).expect("plan");
            assert_eq!(plan.total_cost(), 4.0, "weight {bad_weight} reads as 1.0");
        }
        // Weight 0 is Dijkstra: still optimal. Weight 3 is weighted A*: valid, no cheaper
        // than optimal.
        for weight in [0.0f32, 3.0] {
            let options = PlanOptions {
                heuristic_weight: weight,
                ..no_cache()
            };
            let plan = planner.plan(&start, &goal, &options).expect("plan");
            assert!(plan.validate(&start), "weight {weight}");
            assert!(plan.total_cost() >= 4.0, "weight {weight}");
        }
        // Goal weights shape only the heuristic; a heavy goal fact keeps the optimum.
        let weighted = Goal::new().require(HAS_TABLE, true).with_weight(HAS_TABLE, 50.0);
        let plan = planner.plan(&start, &weighted, &no_cache()).expect("plan");
        assert_eq!(names(&plan), ["pick_up_axe", "chop_tree", "craft_table"]);
        assert_eq!(plan.total_cost(), 4.0);
        // Non-positive or non-finite fact weights are dropped, valid ones stick.
        let goal = Goal::new()
            .with_weight(0, f32::NAN)
            .with_weight(1, -1.0)
            .with_weight(2, 2.5);
        assert_eq!(goal.bool_weight(0), 1.0);
        assert_eq!(goal.bool_weight(1), 1.0);
        assert_eq!(goal.bool_weight(2), 2.5);
        assert_eq!(goal.bool_weight(999), 1.0);
    }

    #[test]
    fn cost_overflow_is_invalid_action_not_a_wrapped_plan() {
        let planner = Planner::new(vec![
            Action::new("huge_a", f32::MAX).sets(0, true),
            Action::new("huge_b", f32::MAX).requires(0, true).sets(1, true),
        ]);
        let one = Goal::new().require(0, true);
        let plan = planner
            .plan(&WorldState::new(), &one, &no_cache())
            .expect("one huge step");
        assert_eq!(plan.total_cost(), f32::MAX);
        let two = Goal::new().require(1, true);
        assert_eq!(
            planner.plan(&WorldState::new(), &two, &no_cache()).err(),
            Some(PlanError::InvalidAction)
        );
    }

    #[test]
    fn world_state_and_condition_basics() {
        // Values outside the mask are canonicalized away.
        let conds = BoolConditions::from_masks([0b0101, 0], [0b1111, 0]);
        assert_eq!(conds.values(), [0b0101, 0]);
        assert_eq!(conds.mask(), [0b0101, 0]);
        let state = WorldState::new().with(0, true).with(2, true).with(65, true);
        assert!(conds.is_satisfied_by(&state));
        assert_eq!(conds.unsatisfied_count(&WorldState::new()), 2);
        assert_eq!(state.count_true(), 3);
        assert!(state.get(65), "fact 65 lives in the second word");
        // Raw parts round-trip.
        let raw = WorldState::from_parts([7, 9], [3; MAX_INT_FACTS]);
        assert_eq!(raw.bits(), [7, 9]);
        assert_eq!(raw.ints(), &[3; MAX_INT_FACTS]);
        assert_eq!(raw.get_int(15), 3);
        // Weighted counting mixes booleans and integer conditions.
        let goal = Goal::from_bool(conds)
            .require_int_weighted(IntCondition::new(0, IntCmp::Ge, 5), 4.0)
            .with_weight(0, 2.0);
        assert_eq!(goal.unsatisfied_count(&WorldState::new()), 3);
        assert_eq!(goal.weighted_unsatisfied(&WorldState::new()), 2.0 + 1.0 + 4.0);
        assert_eq!(goal.weighted_unsatisfied(&state.with_int(0, 5)), 0.0);
        assert!(!goal.is_empty());
        assert!(Goal::new().is_empty());
    }

    #[test]
    fn fact_table_snapshots_and_writes_back_a_blackboard() {
        let mut table = FactTable::new();
        assert_eq!(table.add_bool("has_axe"), Some(0));
        assert_eq!(table.add_bool("has_wood"), Some(1));
        assert_eq!(table.add_bool("has_axe"), Some(0), "a duplicate reuses its index");
        assert_eq!(table.add_int("wood"), Some(0));
        assert_eq!(table.add_int("wood"), Some(0));
        assert_eq!(table.bool_index("has_wood"), Some(1));
        assert_eq!(table.bool_index("missing"), None);
        assert_eq!(table.int_index("wood"), Some(0));
        assert_eq!(table.bool_names().len(), 2);
        assert_eq!(table.int_names().len(), 1);

        let mut blackboard = Blackboard::new();
        blackboard.insert("has_axe", true);
        blackboard.insert("wood", i64::MAX);
        blackboard.insert("unrelated", true);
        let state = table.snapshot(&blackboard);
        assert!(state.get(0));
        assert!(!state.get(1), "a missing key reads as false");
        assert_eq!(state.get_int(0), i32::MAX, "snapshot clamps to i32");

        let changed = state.with(1, true).with_int(0, -2);
        table.write_back(&changed, &mut blackboard);
        assert!(blackboard.get_bool("has_wood"));
        assert_eq!(blackboard.get_int("wood"), -2);
        assert_eq!(table.snapshot(&blackboard), changed);

        // Registration stops at the fact limits instead of wrapping or panicking.
        let mut full = FactTable::new();
        for i in 0..MAX_BOOL_FACTS {
            assert_eq!(full.add_bool(format!("bool_{i}")), Some(i));
        }
        assert_eq!(full.add_bool("one_too_many"), None);
        for i in 0..MAX_INT_FACTS {
            assert_eq!(full.add_int(format!("int_{i}")), Some(i));
        }
        assert_eq!(full.add_int("one_too_many"), None);
    }

    #[test]
    fn empty_plan_and_past_the_end_edges() {
        let planner = woodcutter();
        let done = WorldState::new().with(HAS_TABLE, true);
        let goal = Goal::new().require(HAS_TABLE, true);
        let plan = planner.plan(&done, &goal, &no_cache()).expect("empty plan");
        assert!(plan.validate(&done));
        assert!(
            !plan.validate(&WorldState::new()),
            "no steps left to reach the goal"
        );
        let verdict = plan.validate_remaining(&WorldState::new(), 5);
        assert_eq!(verdict.first_invalid_step, None);
        assert!(!verdict.goal_reached);
        assert_eq!(plan.simulate(&WorldState::new(), 0), Some(WorldState::new()));
        assert_eq!(plan.suffix(0).total_cost(), 0.0);
        assert_eq!(plan.goal(), &goal);
    }

    #[test]
    fn add_action_can_improve_or_poison_a_planner() {
        let goal = Goal::new().require(0, true);
        let mut planner = Planner::new(vec![Action::new("expensive", 9.0).sets(0, true)]);
        let plan = planner
            .plan(&WorldState::new(), &goal, &PlanOptions::default())
            .expect("plan");
        assert_eq!(plan.total_cost(), 9.0);
        planner.add_action(Action::new("cheap", 1.0).sets(0, true));
        assert_eq!(planner.actions().len(), 2);
        assert!(planner.is_valid());
        let plan = planner
            .plan(&WorldState::new(), &goal, &PlanOptions::default())
            .expect("plan");
        assert_eq!(
            names(&plan),
            ["cheap"],
            "the stale cached optimum must not survive"
        );
        planner.add_action(Action::new("", 1.0));
        assert!(!planner.is_valid());
        assert_eq!(
            planner
                .plan(&WorldState::new(), &goal, &PlanOptions::default())
                .err(),
            Some(PlanError::InvalidAction)
        );
    }

    /// A deterministic xorshift64* generator for the oracle test (fixed seeds only).
    struct Rng(u64);

    impl Rng {
        /// The next value; the seed must be non-zero.
        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        /// A value in `0..bound` (`bound` must be positive).
        fn below(&mut self, bound: usize) -> usize {
            (self.next_u64() % bound as u64) as usize
        }

        /// True with probability roughly `percent` in 100.
        fn chance(&mut self, percent: u64) -> bool {
            self.next_u64() % 100 < percent
        }
    }

    /// An independent Dijkstra search over the same actions: the optimality oracle. It
    /// shares only the public `is_applicable` / `apply` / `cost_for` semantics with the
    /// planner, none of the A* machinery.
    fn oracle_cost(actions: &[Action], start: &WorldState, goal: &Goal) -> Option<f32> {
        let mut dist: BTreeMap<WorldState, f32> = BTreeMap::new();
        let mut settled: Vec<WorldState> = Vec::new();
        dist.insert(*start, 0.0);
        loop {
            let mut best: Option<(WorldState, f32)> = None;
            for (state, cost) in &dist {
                if settled.contains(state) {
                    continue;
                }
                if best.is_none_or(|(_, best_cost)| *cost < best_cost) {
                    best = Some((*state, *cost));
                }
            }
            let (state, cost) = best?;
            if goal.is_satisfied_by(&state) {
                return Some(cost);
            }
            settled.push(state);
            for action in actions {
                if !action.is_applicable(&state) {
                    continue;
                }
                let step = action.cost_for(&state).ok()?;
                let next = action.apply(&state);
                if next == state {
                    continue;
                }
                let entry = dist.entry(next).or_insert(f32::INFINITY);
                if cost + step < *entry {
                    *entry = cost + step;
                }
            }
        }
    }

    #[test]
    fn optimal_costs_match_a_dijkstra_oracle_on_200_seeded_domains() {
        const FACTS: usize = 6;
        const COSTS: [f32; 5] = [0.5, 1.0, 1.5, 2.0, 3.0];
        let options = PlanOptions {
            max_nodes: 100_000,
            max_depth: 64,
            use_cache: false,
            ..PlanOptions::default()
        };
        let mut solved = 0usize;
        for seed in 1..=200u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let mut actions = Vec::new();
            for index in 0..9 {
                let mut action = Action::new(format!("a{index}"), COSTS[rng.below(COSTS.len())]);
                for fact in 0..FACTS {
                    if rng.chance(18) {
                        action = action.requires(fact, rng.chance(50));
                    }
                    if rng.chance(22) {
                        action = action.sets(fact, rng.chance(50));
                    }
                }
                actions.push(action);
            }
            let mut start = WorldState::new();
            for fact in 0..FACTS {
                if rng.chance(30) {
                    let _ = start.set(fact, true);
                }
            }
            let mut goal = Goal::new();
            for fact in 0..FACTS {
                if rng.chance(30) {
                    goal = goal.require(fact, rng.chance(50));
                }
            }
            if goal.is_empty() {
                goal = goal.require(0, rng.chance(50));
            }
            let expected = oracle_cost(&actions, &start, &goal);
            let planner = Planner::new(actions);
            match (expected, planner.plan(&start, &goal, &options)) {
                (None, Err(PlanError::NoPlan)) => {}
                (Some(cost), Ok(plan)) => {
                    solved += 1;
                    assert!(
                        (plan.total_cost() - cost).abs() < 1e-3,
                        "seed {seed}: planner {} vs oracle {cost}",
                        plan.total_cost()
                    );
                    assert!(plan.validate(&start), "seed {seed}: plan must simulate");
                }
                (want, got) => panic!("seed {seed}: oracle {want:?}, planner {got:?}"),
            }
        }
        assert!(
            solved >= 25,
            "the random domains should mostly be solvable, got {solved}"
        );
    }

    #[test]
    fn wide_state_performance_guard_128_facts_60_actions() {
        // A 40-step chain over facts 0..40, plus a 20-action decoy chain over facts
        // 100..120 priced out of contention: 60 actions, facts in both bitset words.
        let mut actions = Vec::new();
        for i in 0..40usize {
            let mut action = Action::new(format!("step_{i}"), 1.0).sets(i, true);
            if i > 0 {
                action = action.requires(i - 1, true);
            }
            actions.push(action);
        }
        for j in 0..20usize {
            let mut action = Action::new(format!("decoy_{j}"), 50.0).sets(100 + j, true);
            if j > 0 {
                action = action.requires(99 + j, true);
            }
            actions.push(action);
        }
        assert_eq!(actions.len(), 60);
        let planner = Planner::new(actions);
        let start = WorldState::new().with(65, true).with(127, true);
        let goal = Goal::new().require(39, true);
        let plan = planner
            .plan(&start, &goal, &PlanOptions::default())
            .expect("plan");
        assert_eq!(plan.len(), 40);
        assert_eq!(plan.total_cost(), 40.0);
        assert_eq!(plan.steps()[0].name, "step_0");
        assert_eq!(plan.steps()[39].name, "step_39");
        // The deterministic stand-in for a wall-clock budget: the guided search must not
        // wander the decoy chain or the unused fact space.
        assert!(plan.nodes_expanded() <= 100, "expanded {}", plan.nodes_expanded());
        assert!(plan.validate(&start));
    }

    // ---- Added by the verification lane (sprint-muse-verify-goap) ----

    #[test]
    fn heuristic_never_overestimates_with_fractional_goal_weights() {
        // Ten goal facts at weight 0.1, set two at a time by unit-cost actions: the true
        // remaining cost from k unsatisfied facts is k / 2. In f32 the summed weights round
        // so that W / Wmax lands a hair above the integer (e.g. 0.8000001 / 0.2), and a bare
        // `ceil` would charge one extra step.
        let mut actions = Vec::new();
        let mut goal = Goal::new();
        for pair in 0..5usize {
            actions.push(
                Action::new(format!("pair_{pair}"), 1.0)
                    .sets(2 * pair, true)
                    .sets(2 * pair + 1, true),
            );
        }
        for fact in 0..10usize {
            goal = goal.require(fact, true).with_weight(fact, 0.1);
        }
        let planner = Planner::new(actions);
        let (min_cost, max_weight) = planner.heuristic_scale(&goal);
        let mut state = WorldState::new();
        for done in 0..=5usize {
            let remaining = 5 - done;
            let h = scaled_heuristic(goal.weighted_unsatisfied(&state), max_weight, min_cost, 1.0);
            assert!(h <= remaining as f32, "{remaining} pairs left, h = {h}");
            if done < 5 {
                state = state.with(2 * done, true).with(2 * done + 1, true);
            }
        }
    }

    #[test]
    fn weighted_goals_stay_optimal_against_the_oracle() {
        // The same seeded domains as the unweighted oracle test, with every goal fact given
        // one of a few fractional weights and actions that set several facts at once: goal
        // weights may only change how fast the search runs, never the cost it returns.
        const FACTS: usize = 8;
        const COSTS: [f32; 4] = [0.5, 1.0, 1.5, 2.0];
        const WEIGHTS: [f32; 5] = [0.1, 0.2, 0.3, 0.7, 2.5];
        let options = PlanOptions {
            max_nodes: 100_000,
            use_cache: false,
            ..PlanOptions::default()
        };
        let mut solved = 0usize;
        for seed in 1..=200u64 {
            let mut rng = Rng(seed.wrapping_mul(0xD1B5_4A32_D192_ED03) | 1);
            let mut actions = Vec::new();
            for index in 0..10 {
                let mut action = Action::new(format!("a{index}"), COSTS[rng.below(COSTS.len())]);
                for fact in 0..FACTS {
                    if rng.chance(15) {
                        action = action.requires(fact, rng.chance(50));
                    }
                    if rng.chance(35) {
                        action = action.sets(fact, rng.chance(70));
                    }
                }
                actions.push(action);
            }
            let mut start = WorldState::new();
            for fact in 0..FACTS {
                if rng.chance(25) {
                    let _ = start.set(fact, true);
                }
            }
            let mut goal = Goal::new();
            for fact in 0..FACTS {
                if rng.chance(50) {
                    goal = goal
                        .require(fact, rng.chance(70))
                        .with_weight(fact, WEIGHTS[rng.below(WEIGHTS.len())]);
                }
            }
            if goal.is_empty() {
                goal = goal.require(0, true).with_weight(0, 0.1);
            }
            let expected = oracle_cost(&actions, &start, &goal);
            let planner = Planner::new(actions);
            match (expected, planner.plan(&start, &goal, &options)) {
                (None, Err(PlanError::NoPlan)) => {}
                (Some(cost), Ok(plan)) => {
                    solved += 1;
                    assert!(
                        (plan.total_cost() - cost).abs() < 1e-3,
                        "seed {seed}: planner {} vs oracle {cost}",
                        plan.total_cost()
                    );
                    assert!(plan.validate(&start), "seed {seed}: plan must simulate");
                }
                (want, got) => panic!("seed {seed}: oracle {want:?}, planner {got:?}"),
            }
        }
        assert!(
            solved >= 25,
            "the random domains should mostly be solvable, got {solved}"
        );
    }

    #[test]
    fn integer_goals_stay_optimal_against_the_oracle() {
        // Integer conditions and effects (Set and Add, bounded by Le preconditions so the
        // state space stays finite) on seeded domains, checked against the oracle.
        let options = PlanOptions {
            max_nodes: 200_000,
            use_cache: false,
            ..PlanOptions::default()
        };
        let mut solved = 0usize;
        for seed in 1..=100u64 {
            let mut rng = Rng(seed.wrapping_mul(0x94D0_49BB_1331_11EB) | 1);
            let mut actions = Vec::new();
            for index in 0..8 {
                let cost = [0.5f32, 1.0, 2.0][rng.below(3)];
                let mut action = Action::new(format!("a{index}"), cost);
                for fact in 0..3usize {
                    if rng.chance(25) {
                        action = action.requires(fact, rng.chance(50));
                    }
                    if rng.chance(30) {
                        action = action.sets(fact, rng.chance(60));
                    }
                }
                let int_fact = rng.below(2) as u8;
                let delta = [1i32, 2, -1][rng.below(3)];
                action = action
                    .requires_int(IntCondition::new(int_fact, IntCmp::Le, 5))
                    .requires_int(IntCondition::new(int_fact, IntCmp::Ge, -2))
                    .adds_int(int_fact, delta);
                if rng.chance(20) {
                    action = action.sets_int(1 - int_fact, 0);
                }
                actions.push(action);
            }
            let goal = Goal::new()
                .require_int(IntCondition::new(0, IntCmp::Ge, 1 + rng.below(4) as i32))
                .require_int_weighted(IntCondition::new(1, IntCmp::Ne, 0), 0.3)
                .require(rng.below(3), rng.chance(50));
            let start = WorldState::new();
            let expected = oracle_cost(&actions, &start, &goal);
            let planner = Planner::new(actions);
            match (expected, planner.plan(&start, &goal, &options)) {
                (None, Err(PlanError::NoPlan)) => {}
                (Some(cost), Ok(plan)) => {
                    solved += 1;
                    assert!(
                        (plan.total_cost() - cost).abs() < 1e-3,
                        "seed {seed}: planner {} vs oracle {cost}",
                        plan.total_cost()
                    );
                    assert!(plan.validate(&start), "seed {seed}: plan must simulate");
                }
                (want, got) => panic!("seed {seed}: oracle {want:?}, planner {got:?}"),
            }
        }
        assert!(solved >= 10, "too few solvable integer domains: {solved}");
    }

    #[test]
    fn planner_and_plan_can_live_in_ecs_resources() {
        // Components and resources must be Send + Sync; an AI agent keeps its planner and
        // current plan next to its blackboard.
        fn assert_send_sync<T: Send + Sync + 'static>() {}
        assert_send_sync::<Planner>();
        assert_send_sync::<Plan>();
        assert_send_sync::<Goal>();
        assert_send_sync::<FactTable>();
    }

    /// A dense seeded domain: 60 actions over all 128 boolean facts (two preconditions and
    /// three effects each), a start with about 60 % of the facts true, and a goal made of
    /// the facts a random walk of `walk` applicable actions changed, so the goal is
    /// reachable by construction at no more than the walk's cost. Returns the planner, the
    /// start, the goal and the walk's cost.
    fn dense_domain(seed: u64, walk: usize) -> (Planner, WorldState, Goal, f32) {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let mut actions = Vec::new();
        for index in 0..60usize {
            let cost = [0.5f32, 1.0, 1.5, 2.0][rng.below(4)];
            let mut action = Action::new(format!("a{index}"), cost);
            for _ in 0..2 {
                action = action.requires(rng.below(MAX_BOOL_FACTS), true);
            }
            for _ in 0..3 {
                action = action.sets(rng.below(MAX_BOOL_FACTS), rng.chance(70));
            }
            actions.push(action);
        }
        let mut start = WorldState::new();
        for fact in 0..MAX_BOOL_FACTS {
            if rng.chance(60) {
                let _ = start.set(fact, true);
            }
        }
        let mut state = start;
        let mut walk_cost = 0.0f32;
        for _ in 0..walk {
            let useful: Vec<&Action> = actions
                .iter()
                .filter(|action| action.is_applicable(&state) && action.apply(&state) != state)
                .collect();
            if useful.is_empty() {
                break;
            }
            let action = useful[rng.below(useful.len())];
            walk_cost += action.cost;
            state = action.apply(&state);
        }
        let mut goal = Goal::new();
        for fact in 0..MAX_BOOL_FACTS {
            if state.get(fact) != start.get(fact) {
                goal = goal.require(fact, state.get(fact));
            }
        }
        (Planner::new(actions), start, goal, walk_cost)
    }

    #[test]
    fn performance_dense_random_domain_128_facts_60_actions() {
        // The required performance shape with real branching (60 actions, 128 facts), as a
        // deterministic budget: every reachable-by-construction goal (a four-action walk) is
        // planned inside a generous 100 000-node limit, at no more than the walk that
        // defined it. No wall clock here; `bench_dense_domains` prints timings for the
        // report (the worst of these 20 seeds expands about 13 000 nodes).
        let options = PlanOptions {
            max_nodes: 100_000,
            use_cache: false,
            ..PlanOptions::default()
        };
        for seed in 1..=20u64 {
            let (planner, start, goal, walk_cost) = dense_domain(seed, 4);
            let plan = match planner.plan(&start, &goal, &options) {
                Ok(plan) => plan,
                Err(error) => panic!("seed {seed}: {error:?}"),
            };
            assert!(plan.validate(&start), "seed {seed}");
            assert!(plan.total_cost() <= walk_cost + 1e-3, "seed {seed}");
        }
    }

    /// Timing table for the report: `cargo test -p regia_ai --release --lib
    /// goap::tests::bench_dense_domains -- --ignored --nocapture`.
    #[test]
    #[ignore = "benchmark: prints timings, asserts nothing about time"]
    fn bench_dense_domains() {
        let options = PlanOptions {
            max_nodes: 1_000_000,
            use_cache: false,
            ..PlanOptions::default()
        };
        for walk in [2usize, 4, 6, 8, 10, 12] {
            let mut nodes = Vec::new();
            let mut micros = Vec::new();
            let mut failures = 0usize;
            for seed in 1..=20u64 {
                let (planner, start, goal, _) = dense_domain(seed, walk);
                let clock = std::time::Instant::now();
                let result = planner.plan(&start, &goal, &options);
                micros.push(clock.elapsed().as_micros());
                match result {
                    Ok(plan) => nodes.push(plan.nodes_expanded()),
                    Err(_) => failures += 1,
                }
            }
            nodes.sort_unstable();
            micros.sort_unstable();
            println!(
                "walk {walk:2}: nodes median {} max {} | time us median {} max {} | failures {failures}",
                nodes.get(nodes.len() / 2).copied().unwrap_or(0),
                nodes.last().copied().unwrap_or(0),
                micros[micros.len() / 2],
                micros[micros.len() - 1],
            );
        }
    }

    #[test]
    fn depth_limit_finds_a_costlier_plan_that_fits() {
        // The cheap route reaches the pre-goal state in three steps; a costlier direct action
        // reaches the same state in one. Under max_depth 2 only direct + finish fits, and the
        // cheap deep path to that state must not prune the shallow one.
        let planner = Planner::new(vec![
            Action::new("a1", 1.0).sets(0, true),
            Action::new("a2", 1.0).requires(0, true).sets(1, true),
            Action::new("a3", 1.0).requires(1, true).sets(2, true),
            Action::new("direct", 5.0)
                .sets(0, true)
                .sets(1, true)
                .sets(2, true),
            Action::new("finish", 1.0)
                .requires(0, true)
                .requires(1, true)
                .requires(2, true)
                .sets(3, true),
        ]);
        let goal = Goal::new().require(3, true);
        let unlimited = planner
            .plan(&WorldState::new(), &goal, &no_cache())
            .expect("plan");
        assert_eq!(names(&unlimited), ["a1", "a2", "a3", "finish"]);
        for max_depth in [2usize, 3] {
            let options = PlanOptions {
                max_depth,
                ..no_cache()
            };
            let plan = planner
                .plan(&WorldState::new(), &goal, &options)
                .expect("fits the limit");
            assert_eq!(names(&plan), ["direct", "finish"], "max_depth {max_depth}");
            assert_eq!(plan.total_cost(), 6.0);
        }
        let options = PlanOptions {
            max_depth: 1,
            ..no_cache()
        };
        assert_eq!(
            planner.plan(&WorldState::new(), &goal, &options).err(),
            Some(PlanError::LimitReached)
        );
    }

    #[test]
    fn depth_limited_plans_match_a_depth_bounded_oracle() {
        // On seeded domains, with a binding depth limit, the planner's cost must equal the
        // cheapest plan of at most `max_depth` steps (a layered Bellman-Ford oracle).
        const FACTS: usize = 6;
        const COSTS: [f32; 4] = [0.5, 1.0, 2.0, 4.0];
        let mut checked = 0usize;
        for seed in 1..=200u64 {
            let mut rng = Rng(seed.wrapping_mul(0xA076_1D64_78BD_642F) | 1);
            let mut actions = Vec::new();
            for index in 0..9 {
                let mut action = Action::new(format!("a{index}"), COSTS[rng.below(COSTS.len())]);
                for fact in 0..FACTS {
                    if rng.chance(18) {
                        action = action.requires(fact, rng.chance(50));
                    }
                    if rng.chance(25) {
                        action = action.sets(fact, rng.chance(50));
                    }
                }
                actions.push(action);
            }
            let mut goal = Goal::new();
            for fact in 0..FACTS {
                if rng.chance(35) {
                    goal = goal.require(fact, rng.chance(50));
                }
            }
            if goal.is_empty() {
                goal = goal.require(0, true);
            }
            let start = WorldState::new();
            let max_depth = 1 + rng.below(3);
            // Layered oracle: best[s] after k steps.
            let mut layer: BTreeMap<WorldState, f32> = BTreeMap::new();
            layer.insert(start, 0.0);
            let mut all = layer.clone();
            for _ in 0..max_depth {
                let mut next_layer: BTreeMap<WorldState, f32> = BTreeMap::new();
                for (state, cost) in &layer {
                    for action in &actions {
                        if !action.is_applicable(state) {
                            continue;
                        }
                        let next = action.apply(state);
                        if next == *state {
                            continue;
                        }
                        let total = cost + action.cost;
                        let slot = next_layer.entry(next).or_insert(f32::INFINITY);
                        if total < *slot {
                            *slot = total;
                        }
                    }
                }
                for (state, cost) in &next_layer {
                    let slot = all.entry(*state).or_insert(f32::INFINITY);
                    if *cost < *slot {
                        *slot = *cost;
                    }
                }
                layer = next_layer;
            }
            let expected = all
                .iter()
                .filter(|(state, _)| goal.is_satisfied_by(state))
                .map(|(_, cost)| *cost)
                .min_by(f32::total_cmp);
            let options = PlanOptions {
                max_depth,
                max_nodes: 100_000,
                ..no_cache()
            };
            match (expected, planner_plan(&actions, &start, &goal, &options)) {
                (Some(cost), Ok(plan)) => {
                    checked += 1;
                    assert!(plan.len() <= max_depth, "seed {seed}");
                    assert!(
                        (plan.total_cost() - cost).abs() < 1e-3,
                        "seed {seed} depth {max_depth}: planner {} vs oracle {cost}",
                        plan.total_cost()
                    );
                }
                (None, Err(PlanError::NoPlan | PlanError::LimitReached)) => {}
                (want, got) => panic!("seed {seed} depth {max_depth}: oracle {want:?}, planner {got:?}"),
            }
        }
        assert!(checked >= 25, "too few solvable depth-bounded domains: {checked}");
    }

    fn planner_plan(
        actions: &[Action],
        start: &WorldState,
        goal: &Goal,
        options: &PlanOptions,
    ) -> Result<Plan, PlanError> {
        Planner::new(actions.to_vec()).plan(start, goal, options)
    }
}
