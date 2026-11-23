// SPDX-License-Identifier: MPL-2.0
//! Modifiers and their evaluation (`docs/specs/53-abilities-attributes-effects.md` § 2.3):
//! the five phases, amount sources, level curves, edit gates and magnitude functions.

use std::collections::BTreeMap;

use regia_app::prelude::*;
use serde::{Deserialize, Serialize};

use super::{AttrId, Attributes, EffectId};
use crate::tags::{Tag, TagQuery};

/// How a modifier changes an attribute, and its phase (§ 2.3).
#[derive(Reflect, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModOp {
    /// Phase 1: summed and added to the base.
    AddPre,
    /// Phase 2: fractions summed, applied as `* (1 + sum)`.
    Percent,
    /// Phase 3: multiplied together.
    Scale,
    /// Phase 4: summed and added.
    AddPost,
    /// Phase 5: highest priority wins (ties: earlier applied tick, then lower `EffectId`).
    Replace {
        /// Priority; higher wins.
        priority: i16,
    },
}

impl ModOp {
    /// The phase, 1 to 5.
    #[must_use]
    pub fn phase(self) -> usize {
        match self {
            ModOp::AddPre => 1,
            ModOp::Percent => 2,
            ModOp::Scale => 3,
            ModOp::AddPost => 4,
            ModOp::Replace { .. } => 5,
        }
    }

    /// The lowercase name (`add_pre`, `percent`, `scale`, `add_post`, `replace`).
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            ModOp::AddPre => "add_pre",
            ModOp::Percent => "percent",
            ModOp::Scale => "scale",
            ModOp::AddPost => "add_post",
            ModOp::Replace { .. } => "replace",
        }
    }

    /// Applies the op to `base` as an instant edit (the base-edit meaning of each op).
    #[must_use]
    pub fn apply_to_base(self, base: f32, amount: f32) -> f32 {
        match self {
            ModOp::AddPre | ModOp::AddPost => base + amount,
            ModOp::Percent => base * (1.0 + amount),
            ModOp::Scale => base * amount,
            ModOp::Replace { .. } => amount,
        }
    }
}

/// A curve reference: `table: ""` is the effect file's own `curves` list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LevelCurveRef {
    /// Curve table name, or empty for the effect's inline curves.
    pub table: String,
    /// Curve name in the table.
    pub name: String,
}

/// Which entity a `Stat` amount reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StatSide {
    /// The effect's source.
    Source,
    /// The effect's target.
    Target,
}

/// When a `Stat` amount reads its attribute.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StatRead {
    /// Captured once at application.
    AtApply,
    /// Read every step (the value of the previous step's recompute).
    Live,
}

fn one() -> f32 {
    1.0
}

/// An amount source (§ 2.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Magnitude {
    /// A constant.
    Const(f32),
    /// A curve sampled at the effect level.
    Curve(LevelCurveRef),
    /// `offset + weight * attr.current` of the source or target.
    Stat {
        /// Which entity.
        side: StatSide,
        /// Attribute name.
        attribute: String,
        /// When it is read.
        read: StatRead,
        /// Weight (default 1).
        #[serde(default = "one")]
        weight: f32,
        /// Offset (default 0).
        #[serde(default)]
        offset: f32,
    },
    /// A caller-supplied number keyed by tag.
    Param {
        /// The key.
        key: Tag,
        /// Used when the caller supplies none.
        default: f32,
    },
    /// A registered function, by name.
    Function(String),
}

impl Magnitude {
    /// Whether the amount is read every step.
    #[must_use]
    pub fn is_live(&self) -> bool {
        matches!(
            self,
            Magnitude::Stat {
                read: StatRead::Live,
                ..
            }
        )
    }
}

/// One numeric change to one attribute.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Modifier {
    /// Attribute name.
    pub attribute: String,
    /// The op.
    pub op: ModOp,
    /// The amount.
    pub amount: Magnitude,
    /// Multiply by the stack count.
    #[serde(default)]
    pub per_stack: bool,
    /// Count only while the target's effective tags match.
    #[serde(default)]
    pub when: Option<TagQuery>,
}

/// An inline piecewise-linear curve of an effect file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InlineCurve {
    /// Name.
    pub name: String,
    /// `(x, y)` points, strictly increasing `x`, at least one.
    pub points: Vec<(f32, f32)>,
}

impl InlineCurve {
    /// Samples the curve at `x`: linear between points, clamped outside the range. `None` for an
    /// empty curve.
    #[must_use]
    pub fn sample(&self, x: f32) -> Option<f32> {
        sample_points(&self.points, x)
    }

    /// Whether the points are strictly increasing in `x` and non-empty.
    #[must_use]
    pub fn is_sorted(&self) -> bool {
        !self.points.is_empty() && self.points.windows(2).all(|w| w[0].0 < w[1].0)
    }
}

/// Piecewise-linear sample of sorted points (IEEE `+ - * /` only).
#[must_use]
pub fn sample_points(points: &[(f32, f32)], x: f32) -> Option<f32> {
    let first = points.first()?;
    if x <= first.0 {
        return Some(first.1);
    }
    for pair in points.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        if x <= b.0 {
            let span = b.0 - a.0;
            if span <= 0.0 {
                return Some(b.1);
            }
            let t = (x - a.0) / span;
            return Some(a.1 + (b.1 - a.1) * t);
        }
    }
    points.last().map(|p| p.1)
}

/// Curve lookup for `Magnitude::Curve` with a named table (§ 2.3).
pub trait LevelCurves: Send + Sync + 'static {
    /// The value of curve `name` in `table` at `x`, or `None` when either is missing.
    fn sample(&self, table: &str, name: &str, x: f32) -> Option<f32>;
}

/// The built-in [`LevelCurves`]: named tables of inline curves (empty by default, so every named
/// table reads as missing until a game or the spec 51 adapter supplies one).
#[derive(Debug, Clone, Default)]
pub struct InlineCurveTables {
    tables: BTreeMap<String, Vec<InlineCurve>>,
}

impl InlineCurveTables {
    /// Adds or replaces a table.
    pub fn insert(&mut self, table: &str, curves: Vec<InlineCurve>) {
        self.tables.insert(table.to_owned(), curves);
    }
}

impl LevelCurves for InlineCurveTables {
    fn sample(&self, table: &str, name: &str, x: f32) -> Option<f32> {
        self.tables.get(table)?.iter().find(|c| c.name == name)?.sample(x)
    }
}

/// Spec 51's curve tables as level curves (ADR-1649): `table` is the curve table's asset path
/// (`tables/damage.curves.ron`), sampled with its own interpolation and extrapolation. Effects read
/// a loaded [`CurveTables`](crate::curve_table::CurveTables) resource first and fall back to
/// [`LevelCurvesRes`].
impl LevelCurves for crate::curve_table::CurveTables {
    fn sample(&self, table: &str, name: &str, x: f32) -> Option<f32> {
        self.get(table)?.sample_named(name, x)
    }
}

/// The boxed [`LevelCurves`] resource.
#[derive(Resource)]
pub struct LevelCurvesRes(pub Box<dyn LevelCurves>);

impl Default for LevelCurvesRes {
    fn default() -> Self {
        Self(Box::new(InlineCurveTables::default()))
    }
}

/// An instant edit about to land, shown to every [`EditGates`] entry.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EditRequest {
    /// The entity whose base changes.
    pub target: Entity,
    /// The attribute.
    pub attribute: AttrId,
    /// New base minus old base.
    pub delta: f32,
    /// Who caused it.
    pub source: Option<Entity>,
    /// The effect, when an applied effect caused it.
    pub effect: Option<EffectId>,
}

/// A gate's answer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EditVerdict {
    /// Let it through unchanged.
    Pass,
    /// Multiply the delta.
    Scale(f32),
    /// Drop the edit.
    Refuse,
}

/// An edit gate: a plain function (a shield, "no healing while diseased").
pub type EditGateFn = fn(&World, &EditRequest) -> EditVerdict;

/// Registered edit gates, in registration order, with the registering owner (§ 3.3).
#[derive(Resource, Debug, Clone, Default)]
pub struct EditGates {
    entries: Vec<(TypeOwner, EditGateFn)>,
}

impl EditGates {
    /// Registers a gate for `owner`.
    pub fn register(&mut self, owner: TypeOwner, gate: EditGateFn) {
        self.entries.push((owner, gate));
    }

    /// Drops every gate `owner` registered (hot-reload teardown).
    pub fn drop_owner(&mut self, owner: TypeOwner) {
        self.entries.retain(|(o, _)| *o != owner);
    }

    /// Number of gates.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether there are none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Runs `request` through every gate: the final delta, or `None` when refused.
    #[must_use]
    pub fn run(&self, world: &World, request: &EditRequest) -> Option<f32> {
        let mut delta = request.delta;
        for (_, gate) in &self.entries {
            let current = EditRequest { delta, ..*request };
            match gate(world, &current) {
                EditVerdict::Pass => {}
                EditVerdict::Scale(f) => delta *= f,
                EditVerdict::Refuse => return None,
            }
        }
        Some(delta)
    }
}

/// What a magnitude function may read.
#[derive(Debug, Clone, Copy)]
pub struct MagnitudeInput<'a> {
    /// The source's attributes, if any.
    pub source: Option<&'a Attributes>,
    /// The target's attributes.
    pub target: Option<&'a Attributes>,
    /// Effect level.
    pub level: u32,
    /// Stack count.
    pub stacks: u16,
}

/// A registered magnitude function.
pub type MagnitudeFn = fn(&MagnitudeInput) -> f32;

/// Registered magnitude functions by name, with the registering owner (§ 3.3).
#[derive(Resource, Debug, Clone, Default)]
pub struct MagnitudeFns {
    entries: BTreeMap<String, (TypeOwner, MagnitudeFn)>,
}

impl MagnitudeFns {
    /// Registers `f` under `name` for `owner` (replacing an older entry).
    pub fn register(&mut self, owner: TypeOwner, name: &str, f: MagnitudeFn) {
        self.entries.insert(name.to_owned(), (owner, f));
    }

    /// Drops every function `owner` registered.
    pub fn drop_owner(&mut self, owner: TypeOwner) {
        self.entries.retain(|_, (o, _)| *o != owner);
    }

    /// The function named `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<MagnitudeFn> {
        self.entries.get(name).map(|(_, f)| *f)
    }

    /// Registered names, sorted.
    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        self.entries.keys().map(String::as_str).collect()
    }
}

/// Builds a run-state value's default RON text.
pub type RunStateDefaultFn = fn() -> Option<String>;

/// Registered run-state types by name, with the registering owner (§ 2.6, § 3.3). Values are
/// kept on the grant as RON text, so a grant stays plain, saveable data.
#[derive(Resource, Debug, Clone, Default)]
pub struct RunStateTypes {
    entries: BTreeMap<String, (TypeOwner, RunStateDefaultFn)>,
}

impl RunStateTypes {
    /// Registers `T` under `name` for `owner`.
    pub fn register<T: Serialize + Default>(&mut self, owner: TypeOwner, name: &str) {
        fn default_text<T: Serialize + Default>() -> Option<String> {
            ron::to_string(&T::default()).ok()
        }
        self.entries
            .insert(name.to_owned(), (owner, default_text::<T> as RunStateDefaultFn));
    }

    /// Drops every type `owner` registered.
    pub fn drop_owner(&mut self, owner: TypeOwner) {
        self.entries.retain(|_, (o, _)| *o != owner);
    }

    /// Whether `name` is registered.
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.entries.contains_key(name)
    }

    /// The default RON text of `name`.
    #[must_use]
    pub fn default_text(&self, name: &str) -> Option<String> {
        self.entries.get(name).and_then(|(_, f)| f())
    }
}

/// One live modifier ready for evaluation (amount already resolved, stacks applied by
/// [`evaluate`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LiveMod {
    /// The effect that holds it.
    pub effect: EffectId,
    /// When that effect was applied.
    pub applied_tick: u64,
    /// The op.
    pub op: ModOp,
    /// The resolved amount (one stack).
    pub amount: f32,
    /// Stack count of the effect.
    pub stacks: u16,
    /// Whether the amount is multiplied by the stack count.
    pub per_stack: bool,
}

impl LiveMod {
    /// The amount with stacks applied (§ 2.3): repeated multiplication for `Scale`.
    #[must_use]
    pub fn stacked(&self) -> f32 {
        if !self.per_stack {
            return self.amount;
        }
        match self.op {
            ModOp::Replace { .. } => self.amount,
            ModOp::Scale => {
                let mut v = 1.0f32;
                for _ in 0..self.stacks {
                    v *= self.amount;
                }
                v
            }
            _ => self.amount * f32::from(self.stacks),
        }
    }
}

/// One contributing modifier in an [`AttributeBreakdown`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Contribution {
    /// The effect.
    pub effect: EffectId,
    /// The op.
    pub op: ModOp,
    /// The amount per stack.
    pub amount: f32,
    /// The stack count.
    pub stacks: u16,
    /// The amount used (stacks applied).
    pub contribution: f32,
}

/// One phase of an [`AttributeBreakdown`].
#[derive(Debug, Clone, PartialEq)]
pub struct PhaseBreakdown {
    /// `add_pre`, `percent`, `scale`, `add_post` or `replace`.
    pub phase: &'static str,
    /// Contributing modifiers in evaluation order.
    pub modifiers: Vec<Contribution>,
    /// The value after the phase.
    pub value_after: f32,
}

/// How an attribute's `current` was computed (§ 3.2 `explain_attribute`).
#[derive(Debug, Clone, PartialEq)]
pub struct AttributeBreakdown {
    /// The attribute.
    pub attribute: AttrId,
    /// Its name.
    pub name: String,
    /// The base the phases started from.
    pub base: f32,
    /// The five phases, in order.
    pub phases: Vec<PhaseBreakdown>,
    /// The winning `Replace`, if any.
    pub replace_winner: Option<EffectId>,
    /// The value before the clamp.
    pub unclamped: f32,
    /// The clamp range applied, `(min, effective max)`.
    pub clamp: (f32, f32),
    /// The final value.
    pub current: f32,
}

/// Evaluates the five phases over `mods` (in ascending `EffectId`, then modifier index), then
/// clamps. Fills `breakdown` when given.
pub fn evaluate(
    base: f32,
    mods: &[LiveMod],
    min: f32,
    max: f32,
    mut breakdown: Option<&mut AttributeBreakdown>,
) -> f32 {
    let mut v = base;
    let mut record = |phase: &'static str, op_phase: usize, v: f32| {
        if let Some(b) = breakdown.as_deref_mut() {
            b.phases.push(PhaseBreakdown {
                phase,
                modifiers: mods
                    .iter()
                    .filter(|m| m.op.phase() == op_phase)
                    .map(|m| Contribution {
                        effect: m.effect,
                        op: m.op,
                        amount: m.amount,
                        stacks: m.stacks,
                        contribution: m.stacked(),
                    })
                    .collect(),
                value_after: v,
            });
        }
    };
    let mut sum = 0.0f32;
    for m in mods.iter().filter(|m| m.op == ModOp::AddPre) {
        sum += m.stacked();
    }
    v += sum;
    record("add_pre", 1, v);
    let mut pct = 0.0f32;
    for m in mods.iter().filter(|m| m.op == ModOp::Percent) {
        pct += m.stacked();
    }
    v *= 1.0 + pct;
    record("percent", 2, v);
    let mut prod = 1.0f32;
    for m in mods.iter().filter(|m| m.op == ModOp::Scale) {
        prod *= m.stacked();
    }
    v *= prod;
    record("scale", 3, v);
    let mut post = 0.0f32;
    for m in mods.iter().filter(|m| m.op == ModOp::AddPost) {
        post += m.stacked();
    }
    v += post;
    record("add_post", 4, v);
    let mut winner: Option<(i16, u64, EffectId, f32)> = None;
    for m in mods {
        if let ModOp::Replace { priority } = m.op {
            let better = match winner {
                None => true,
                Some((p, tick, id, _)) => {
                    priority > p || (priority == p && (m.applied_tick, m.effect) < (tick, id))
                }
            };
            if better {
                winner = Some((priority, m.applied_tick, m.effect, m.amount));
            }
        }
    }
    if let Some((_, _, _, amount)) = winner {
        v = amount;
    }
    record("replace", 5, v);
    let hi = if max < min { min } else { max };
    let current = if v.is_nan() { min } else { v.clamp(min, hi) };
    if let Some(b) = breakdown {
        b.base = base;
        b.replace_winner = winner.map(|w| w.2);
        b.unclamped = v;
        b.clamp = (min, hi);
        b.current = current;
    }
    current
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(effect: u64, op: ModOp, amount: f32) -> LiveMod {
        LiveMod {
            effect: EffectId(effect),
            applied_tick: effect,
            op,
            amount,
            stacks: 1,
            per_stack: false,
        }
    }

    #[test]
    fn stacks_use_repeated_multiplication_for_scale() {
        let mut s = m(1, ModOp::Scale, 2.0);
        s.stacks = 3;
        s.per_stack = true;
        assert_eq!(s.stacked(), 8.0);
        let mut a = m(1, ModOp::AddPre, 2.5);
        a.stacks = 2;
        a.per_stack = true;
        assert_eq!(a.stacked(), 5.0);
    }

    #[test]
    fn curves_interpolate_and_clamp() {
        let c = InlineCurve {
            name: "p".into(),
            points: vec![(1.0, -2.0), (10.0, -9.0)],
        };
        assert_eq!(c.sample(0.0), Some(-2.0));
        assert_eq!(c.sample(1.0), Some(-2.0));
        assert_eq!(c.sample(10.0), Some(-9.0));
        assert_eq!(c.sample(20.0), Some(-9.0));
        assert_eq!(c.sample(5.5), Some(-5.5));
        assert!(c.is_sorted());
    }

    #[test]
    fn op_order_matches_the_spec_example() {
        let mods = [
            m(1, ModOp::AddPre, 5.0),
            m(2, ModOp::Percent, 0.5),
            m(3, ModOp::Percent, 0.25),
            m(4, ModOp::Scale, 2.0),
            m(5, ModOp::Scale, 1.5),
            m(6, ModOp::AddPost, -3.0),
        ];
        assert_eq!(evaluate(10.0, &mods, 0.0, 1000.0, None), 75.75);
    }
}
