// SPDX-License-Identifier: MPL-2.0
//! Attributes (`docs/specs/53-abilities-attributes-effects.md` § 2.2): named `f32` values with a
//! durable `base`, a derived `current`, clamps, `max_from` links and derived values.

use std::sync::Arc;

use regia_app::prelude::*;
use regia_ecs::ecs::change_detection::Tick;
use serde::{Deserialize, Serialize};

use super::AbilityError;
use crate::tags::fnv1a64;

/// Stable id of an attribute: FNV-1a 64 of its name (the same in every run).
#[derive(
    Reflect, Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize,
)]
pub struct AttrId(pub u64);

impl AttrId {
    /// The id of the attribute named `name`.
    #[must_use]
    pub const fn of(name: &str) -> Self {
        Self(fnv1a64(name.as_bytes()))
    }
}

/// A derived attribute: `base = offset + sum(input.current * weight)` every step.
#[derive(Reflect, Debug, Clone, PartialEq, Default)]
pub struct Derived {
    /// Inputs and their weights, in declaration order.
    pub inputs: Vec<(AttrId, f32)>,
    /// Constant added to the weighted sum.
    pub offset: f32,
}

/// One attribute of an entity.
#[derive(Reflect, Debug, Clone, PartialEq)]
pub struct Attribute {
    /// Id (FNV-1a of `name`).
    pub id: AttrId,
    /// Name, `[a-z][a-z0-9_]{0,47}`.
    pub name: String,
    /// Durable value; instant effects and pulses change it.
    pub base: f32,
    /// The value every reader uses: `base` shaped by the live modifiers, clamped.
    pub current: f32,
    /// Lower bound.
    pub min: f32,
    /// Upper bound.
    pub max: f32,
    /// Another attribute whose `current` caps this one's upper bound.
    pub max_from: Option<AttrId>,
    /// When set, `base` is recomputed from other attributes every step.
    pub derived: Option<Derived>,
}

impl Attribute {
    /// A plain attribute with `base == current`.
    #[must_use]
    pub fn new(name: &str, base: f32, min: f32, max: f32) -> Self {
        Self {
            id: AttrId::of(name),
            name: name.to_owned(),
            base,
            current: base,
            min,
            max,
            max_from: None,
            derived: None,
        }
    }

    /// Builder: cap the upper bound by `other.current`.
    #[must_use]
    pub fn with_max_from(mut self, other: &str) -> Self {
        self.max_from = Some(AttrId::of(other));
        self
    }

    /// Builder: make the attribute derived.
    #[must_use]
    pub fn with_derived(mut self, inputs: &[(&str, f32)], offset: f32) -> Self {
        self.derived = Some(Derived {
            inputs: inputs.iter().map(|(n, w)| (AttrId::of(n), *w)).collect(),
            offset,
        });
        self
    }
}

/// Whether `name` is a valid attribute name (`[a-z][a-z0-9_]{0,47}`).
#[must_use]
pub fn valid_attribute_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 48
        && bytes[0].is_ascii_lowercase()
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_')
}

/// Which value an [`AttributeChanged`] message reports.
#[derive(Reflect, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttrLane {
    /// The durable `base`.
    Base,
    /// The derived `current`.
    Current,
}

/// Written in `AbilitySet::Notify` when an attribute's `current` or `base` changed this step.
#[derive(Message, Debug, Clone, Copy, PartialEq)]
pub struct AttributeChanged {
    /// The entity.
    pub entity: Entity,
    /// The attribute.
    pub attribute: AttrId,
    /// Which value changed.
    pub lane: AttrLane,
    /// Value at the previous report.
    pub old: f32,
    /// Value now.
    pub new: f32,
    /// The source of the last base edit, when known.
    pub source: Option<Entity>,
}

/// All attributes of one entity, in declaration order.
#[derive(Component, Reflect, Debug, Clone, Default)]
#[reflect(Component, Default)]
pub struct Attributes {
    list: Vec<Attribute>,
    /// Evaluation order (indices into `list`): derived inputs and `max_from` targets first.
    #[reflect(ignore)]
    pub(crate) order: Box<[u16]>,
    /// False when the entity must be recomputed this step (dirty flag, § 10).
    #[reflect(ignore)]
    pub(crate) clean: bool,
    /// `(id, base, current)` at the last `AttributeChanged` report, per attribute.
    #[reflect(ignore)]
    pub(crate) reported: Report,
    /// Source of the last base edit.
    #[reflect(ignore)]
    pub(crate) last_source: Option<Entity>,
    /// Whether the last recompute saw only fixed modifiers (`EffectEntry::fixed_mods`).
    #[reflect(ignore)]
    pub(crate) fixed_mods: bool,
    /// The change tick of this entity's `AppliedEffects` before and at the effect pass's last
    /// write of nothing but clocks; that write has a tick of its own (ADR-2012).
    #[reflect(ignore)]
    pub(crate) clock_write: Option<(Tick, Tick)>,
}

impl Attributes {
    /// An empty set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds a set from a list, checking names, duplicates, ranges and cycles.
    pub fn from_list(list: impl IntoIterator<Item = Attribute>) -> Result<Self, AbilityError> {
        let mut set = Self::new();
        for attribute in list {
            set.push(attribute)?;
        }
        Ok(set)
    }

    /// Adds an attribute. Fails with `attribute_duplicate`, `attribute_range`,
    /// `attribute_not_finite` or `attribute_cycle`; on failure the set is unchanged.
    pub fn push(&mut self, mut attribute: Attribute) -> Result<(), AbilityError> {
        if self.list.iter().any(|a| a.id == attribute.id) {
            return Err(AbilityError::AttributeDuplicate(attribute.name));
        }
        if !valid_attribute_name(&attribute.name) {
            return Err(AbilityError::BadName(attribute.name));
        }
        if !(attribute.min.is_finite() && attribute.max.is_finite() && attribute.base.is_finite()) {
            return Err(AbilityError::AttributeNotFinite(attribute.name));
        }
        if attribute.min > attribute.max {
            return Err(AbilityError::AttributeRange(attribute.name));
        }
        attribute.id = AttrId::of(&attribute.name);
        attribute.base = attribute.base.clamp(attribute.min, attribute.max);
        attribute.current = attribute.base;
        self.list.push(attribute);
        match topo_order(&self.list) {
            Ok(order) => {
                self.order = order.into_boxed_slice();
                self.clean = false;
                Ok(())
            }
            Err(name) => {
                self.list.pop();
                Err(AbilityError::AttributeCycle(name))
            }
        }
    }

    /// Every attribute in declaration order.
    #[must_use]
    pub fn list(&self) -> &[Attribute] {
        &self.list
    }

    /// Number of attributes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.list.len()
    }

    /// Whether the set is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    /// Index of `id`.
    #[must_use]
    pub fn index_of(&self, id: AttrId) -> Option<usize> {
        self.list.iter().position(|a| a.id == id)
    }

    /// The attribute `id`.
    #[must_use]
    pub fn attribute(&self, id: AttrId) -> Option<&Attribute> {
        self.list.iter().find(|a| a.id == id)
    }

    /// The attribute named `name`.
    #[must_use]
    pub fn by_name(&self, name: &str) -> Option<&Attribute> {
        self.attribute(AttrId::of(name))
    }

    /// `current` of `id`.
    #[must_use]
    pub fn get(&self, id: AttrId) -> Option<f32> {
        self.attribute(id).map(|a| a.current)
    }

    /// `base` of `id`.
    #[must_use]
    pub fn base(&self, id: AttrId) -> Option<f32> {
        self.attribute(id).map(|a| a.base)
    }

    /// The upper bound of the attribute at `index` this step: `min(max, other.current)` with
    /// `max_from`.
    pub(crate) fn effective_max(&self, index: usize) -> f32 {
        let attribute = &self.list[index];
        let mut max = attribute.max;
        if let Some(other) = attribute.max_from.and_then(|id| self.get(id)) {
            max = max.min(other);
        }
        max.max(attribute.min)
    }

    /// Sets `base` directly (no edit gates), clamped. Refuses derived attributes
    /// (`attribute_derived`) and non-finite values (`attribute_not_finite`). Returns `(old, new)`.
    pub fn set_base(&mut self, id: AttrId, value: f32) -> Result<(f32, f32), AbilityError> {
        let index = self.index_of(id).ok_or(AbilityError::UnknownAttribute(id.0))?;
        if self.list[index].derived.is_some() {
            return Err(AbilityError::AttributeDerived(self.list[index].name.clone()));
        }
        if !value.is_finite() {
            return Err(AbilityError::AttributeNotFinite(self.list[index].name.clone()));
        }
        let max = self.effective_max(index);
        let attribute = &mut self.list[index];
        let old = attribute.base;
        attribute.base = value.clamp(attribute.min, max);
        self.clean = false;
        Ok((old, self.list[index].base))
    }

    /// Mutable access for the recompute pass.
    /// Sets `current` directly, for a test that pins a state a step would not produce.
    #[doc(hidden)]
    pub fn set_current_for_test(&mut self, id: AttrId, value: f32) {
        if let Some(index) = self.index_of(id) {
            self.list[index].current = value;
        }
    }

    /// Whether the last `AttributeChanged` report saw exactly the values held now: the same
    /// attributes in the same order, bases and currents equal to the bit.
    pub(crate) fn matches_report(&self) -> bool {
        self.list.len() == self.reported.len()
            && self.list.iter().enumerate().all(|(i, a)| {
                self.reported
                    .row(i)
                    .is_some_and(|row| same_row(row, (a.id, a.base, a.current)))
            })
    }

    pub(crate) fn list_mut(&mut self) -> &mut [Attribute] {
        &mut self.list
    }

    /// The evaluation order, rebuilt when it is stale (a loaded save has none).
    pub(crate) fn ensure_order(&mut self) {
        if self.order.len() != self.list.len() {
            // A loaded set was valid when saved; on a cycle fall back to declaration order.
            self.order = topo_order(&self.list)
                .map(Vec::into_boxed_slice)
                .unwrap_or_else(|_| (0..self.list.len() as u16).collect());
        }
    }

    /// Marks the set for recompute in the next step.
    pub fn mark_dirty(&mut self) {
        self.clean = false;
    }
}

/// One attribute's `(id, base, current)` at an `AttributeChanged` report.
pub(crate) type ReportRow = (AttrId, f32, f32);

/// Whether two report rows hold the same id and the same values to the bit.
fn same_row(a: ReportRow, b: ReportRow) -> bool {
    a.0 == b.0 && a.1.to_bits() == b.1.to_bits() && a.2.to_bits() == b.2.to_bits()
}

/// Snapshots the notify pass made recently, offered to the next entities that report the same
/// values (spec 53 § 10 memory budget, ADR-2426): entities spawned from one template share one.
pub(crate) const REPORT_CACHE: usize = 8;

/// How `now` differs from `snapshot`: `Some(None)` not at all, `Some(Some(row))` in the one row
/// given as `(index, base, current)`, `None` in more rows or in its ids.
fn one_row_off(snapshot: &[ReportRow], now: &[ReportRow]) -> Option<Option<(u16, f32, f32)>> {
    if snapshot.len() != now.len() {
        return None;
    }
    let mut own = None;
    for (i, (a, b)) in snapshot.iter().zip(now).enumerate() {
        if a.0 != b.0 {
            return None;
        }
        if !same_row(*a, *b) {
            if own.is_some() {
                return None;
            }
            own = Some((u16::try_from(i).ok()?, b.1, b.2));
        }
    }
    Some(own)
}

/// What the notify pass last reported for one entity (`Attributes::reported`): a snapshot shared
/// by every entity that reported the same values, and at most one row of the entity's own that
/// differs from it (spec 53 § 10: 1,000 idle entities in 64 KiB, ADR-2426). Reads see exactly the
/// rows a per-entity copy held; only where they are stored differs.
#[derive(Debug, Clone, Default)]
pub(crate) struct Report {
    /// The snapshot; `None` before the first report (and after a load: `reported` is not saved).
    shared: Option<Arc<[ReportRow]>>,
    /// `(index, base, current)` of the one row that differs from `shared`.
    own: Option<(u16, f32, f32)>,
}

impl Report {
    /// Rows in the report.
    pub(crate) fn len(&self) -> usize {
        self.shared.as_ref().map_or(0, |s| s.len())
    }

    /// Row `index`.
    pub(crate) fn row(&self, index: usize) -> Option<ReportRow> {
        let (id, base, current) = *self.shared.as_ref()?.get(index)?;
        Some(match self.own {
            Some((i, b, c)) if usize::from(i) == index => (id, b, c),
            _ => (id, base, current),
        })
    }

    /// `(base, current)` of the first row of attribute `id`.
    pub(crate) fn find(&self, id: AttrId) -> Option<(f32, f32)> {
        let index = self.shared.as_ref()?.iter().position(|row| row.0 == id)?;
        self.row(index).map(|(_, b, c)| (b, c))
    }

    /// Replaces the report with `now`. In order: a snapshot only this entity holds is written in
    /// place; one shared with others keeps being shared when `now` differs from it in at most one
    /// row; else a recent snapshot equal to `now` (or off by one row) is shared; else a new one is
    /// made and offered to the next entities. No allocation unless a new snapshot is made.
    pub(crate) fn store(&mut self, now: &[ReportRow], recent: &mut Vec<Arc<[ReportRow]>>) {
        if let Some(shared) = &mut self.shared
            && shared.len() == now.len()
            && shared.iter().zip(now).all(|(a, b)| a.0 == b.0)
        {
            // Held by this entity and the recent list alone: leave the list, write in place.
            if Arc::strong_count(shared) == 2
                && let Some(at) = recent.iter().position(|r| Arc::ptr_eq(r, shared))
            {
                recent.remove(at);
            }
            if let Some(rows) = Arc::get_mut(shared) {
                rows.copy_from_slice(now);
                self.own = None;
                return;
            }
            if let Some(own) = one_row_off(shared, now) {
                self.own = own;
                return;
            }
        }
        // A recent snapshot equal to `now`, else one off by a single row.
        let near = recent
            .iter()
            .find(|r| one_row_off(r, now) == Some(None))
            .or_else(|| recent.iter().find(|r| one_row_off(r, now).is_some()));
        if let Some(found) = near {
            self.own = one_row_off(found, now).flatten();
            self.shared = Some(Arc::clone(found));
            return;
        }
        self.own = None;
        let made: Arc<[ReportRow]> = Arc::from(now);
        if recent.len() >= REPORT_CACHE {
            recent.remove(0);
        }
        recent.push(Arc::clone(&made));
        self.shared = Some(made);
    }
}

/// Topological order of `list` over derived-input and `max_from` edges (an attribute comes after
/// everything it reads), ties by declaration order. `Err(name)` names an attribute on a cycle.
fn topo_order(list: &[Attribute]) -> Result<Vec<u16>, String> {
    let n = list.len();
    let deps = |i: usize| -> Vec<usize> {
        let a = &list[i];
        let mut out: Vec<usize> = Vec::new();
        if let Some(derived) = &a.derived {
            for (input, _) in &derived.inputs {
                if let Some(j) = list.iter().position(|b| b.id == *input) {
                    out.push(j);
                }
            }
        }
        if let Some(other) = a.max_from
            && let Some(j) = list.iter().position(|b| b.id == other)
        {
            out.push(j);
        }
        out
    };
    let mut done = vec![false; n];
    let mut order = Vec::with_capacity(n);
    while order.len() < n {
        let next = (0..n).find(|&i| !done[i] && deps(i).iter().all(|&j| done[j] && j != i));
        match next {
            Some(i) => {
                done[i] = true;
                order.push(i as u16);
            }
            None => {
                let stuck = (0..n).find(|&i| !done[i]).unwrap_or(0);
                return Err(list.get(stuck).map(|a| a.name.clone()).unwrap_or_default());
            }
        }
    }
    Ok(order)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_ranges_duplicates_and_cycles_are_checked() {
        let mut set = Attributes::new();
        set.push(Attribute::new("health", 10.0, 0.0, 100.0)).expect("ok");
        assert_eq!(
            set.push(Attribute::new("health", 1.0, 0.0, 1.0))
                .map_err(|e| e.reason()),
            Err("attribute_duplicate")
        );
        assert_eq!(
            set.push(Attribute::new("bad", 1.0, 5.0, 1.0))
                .map_err(|e| e.reason()),
            Err("attribute_range")
        );
        let mut cyc = Attributes::new();
        cyc.push(Attribute::new("a", 1.0, 0.0, 10.0).with_max_from("b"))
            .expect("dangling link is fine until b exists");
        assert_eq!(
            cyc.push(Attribute::new("b", 1.0, 0.0, 10.0).with_max_from("a"))
                .map_err(|e| e.reason()),
            Err("attribute_cycle")
        );
        assert_eq!(cyc.len(), 1, "a failed push leaves the set unchanged");
    }

    /// ADR-2426: a shared report reads exactly like a plain per-entity copy, whatever the entities
    /// report and in whatever order (that alike entities share is the next test).
    #[test]
    fn a_shared_report_reads_like_a_copy_of_its_own() {
        let mut rng = regia_core::math::Rng::new(2426);
        let ids: Vec<AttrId> = ["a", "b", "c", "d"].iter().map(|&n| AttrId::of(n)).collect();
        let mut reports: Vec<Report> = vec![Report::default(); 24];
        let mut copies: Vec<Vec<ReportRow>> = vec![Vec::new(); 24];
        let mut recent = Vec::new();
        let mut now = Vec::new();
        for round in 0..2_000u32 {
            let e = rng.range(0..reports.len());
            now.clear();
            // Values from a small set, so entities often agree, or differ in one or two rows.
            let n = if rng.range(0..40u32) == 0 { 3 } else { 4 };
            for &id in &ids[..n] {
                let v = rng.range(0..3u32) as f32;
                now.push((id, v, if rng.range(0..4u32) == 0 { v + 0.5 } else { v }));
            }
            if round % 97 == 0 {
                // A snapshot shared by a clone of the component.
                reports.push(reports[e].clone());
                copies.push(copies[e].clone());
            }
            reports[e].store(&now, &mut recent);
            copies[e].clone_from(&now);
            assert!(recent.len() <= REPORT_CACHE);
            for (report, copy) in reports.iter().zip(&copies) {
                assert_eq!(report.len(), copy.len());
                for (i, row) in copy.iter().enumerate() {
                    let got = report.row(i).expect("row");
                    assert!(same_row(got, *row), "round {round}: {got:?} != {row:?}");
                    let (b, c) = report.find(row.0).expect("find");
                    assert_eq!((b.to_bits(), c.to_bits()), (row.1.to_bits(), row.2.to_bits()));
                }
                assert_eq!(report.row(copy.len()), None);
            }
        }
    }

    /// ADR-2426: an entity that keeps changing writes its own snapshot in place (§ 9: no
    /// allocation per step), and a template's entities share one.
    #[test]
    fn a_report_is_shared_by_alike_entities_and_written_in_place_when_alone() {
        let rows = |v: f32| {
            vec![
                (AttrId::of("a"), v, v),
                (AttrId::of("b"), 1.0, 1.0),
                (AttrId::of("c"), 2.0, 2.0),
            ]
        };
        let mut recent = Vec::new();
        let mut alike: Vec<Report> = vec![Report::default(); 10];
        for r in &mut alike {
            r.store(&rows(5.0), &mut recent);
        }
        assert_eq!(recent.len(), 1, "one snapshot for ten alike entities");
        let first = alike[0].shared.clone().expect("shared");
        assert!(
            alike
                .iter()
                .all(|r| r.shared.as_ref().is_some_and(|s| Arc::ptr_eq(s, &first)))
        );
        drop(first);
        // One row off: still shared, with a row of its own.
        alike[3].store(&rows(6.0), &mut recent);
        assert_eq!(alike[3].own, Some((0, 6.0, 6.0)));
        assert_eq!(recent.len(), 1);
        // Two rows off: a snapshot of its own, then written in place.
        let mut two = rows(7.0);
        two[2].2 = 9.0;
        alike[4].store(&two, &mut recent);
        let own = alike[4].shared.clone().expect("own");
        assert_eq!(recent.len(), 2);
        drop(own);
        for v in 0..5 {
            two[1].1 = v as f32;
            alike[4].store(&two, &mut recent);
        }
        assert_eq!(
            recent.len(),
            1,
            "its snapshot left the recent list to be written in place"
        );
        assert_eq!(alike[4].row(1), Some((AttrId::of("b"), 4.0, 1.0)));
    }

    #[test]
    fn derived_inputs_come_first_in_the_order() {
        let set = Attributes::from_list([
            Attribute::new("armor", 0.0, 0.0, 100.0).with_derived(&[("strength", 2.0)], 0.0),
            Attribute::new("strength", 10.0, 0.0, 100.0),
        ])
        .expect("ok");
        assert_eq!(&*set.order, &[1, 0]);
    }
}
