// SPDX-License-Identifier: MPL-2.0
//! [`TagContainer`]: a small sorted set of tags with hierarchical and exact queries.

use regia_app::prelude::*;
use serde::{Deserialize, Serialize};

use super::name::{Tag, TagError};
use super::query::TagQuery;

/// A small set of [`Tag`]s, kept sorted by name with no duplicates, so iteration, saving and
/// hashing are the same on every run.
///
/// The container stores only the tags that were added (the *explicit* tags). Parents are implied:
/// a container holding `Combat.Damage.Fire` answers `has(Combat.Damage)` and `has(Combat)` with
/// `true`, and `has_exact(Combat.Damage)` with `false`. Lookups scan the set; containers are meant
/// to hold a handful of tags, not thousands.
///
/// As a component it is reflected and serializes as a list of names:
/// `(tags: ["Combat.Damage.Fire", "Status.Burning"])`.
#[derive(Component, Reflect, Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[reflect(Component, Default, Debug, PartialEq)]
#[serde(from = "RawContainer", into = "RawContainer")]
#[require(super::TagSnapshot)]
pub struct TagContainer {
    tags: Vec<Tag>,
}

#[derive(Serialize, Deserialize)]
struct RawContainer {
    tags: Vec<Tag>,
}

impl From<RawContainer> for TagContainer {
    fn from(raw: RawContainer) -> Self {
        raw.tags.into_iter().collect()
    }
}

impl From<TagContainer> for RawContainer {
    fn from(container: TagContainer) -> Self {
        Self { tags: container.tags }
    }
}

impl FromIterator<Tag> for TagContainer {
    fn from_iter<I: IntoIterator<Item = Tag>>(iter: I) -> Self {
        let mut tags: Vec<Tag> = iter.into_iter().collect();
        tags.sort();
        tags.dedup();
        Self { tags }
    }
}

impl Extend<Tag> for TagContainer {
    fn extend<I: IntoIterator<Item = Tag>>(&mut self, iter: I) {
        for tag in iter {
            self.add(tag);
        }
    }
}

impl TagContainer {
    /// An empty container.
    pub fn new() -> Self {
        Self::default()
    }

    /// A container of the named tags; the first invalid name is the error.
    pub fn from_names<'a>(names: impl IntoIterator<Item = &'a str>) -> Result<Self, TagError> {
        names.into_iter().map(Tag::new).collect()
    }

    /// The explicit tags, sorted by name.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = Tag> + '_ {
        self.tags.iter().copied()
    }

    /// The explicit tags' names, sorted.
    pub fn names(&self) -> Vec<&'static str> {
        self.tags.iter().map(Tag::name).collect()
    }

    /// The number of explicit tags.
    pub fn len(&self) -> usize {
        self.tags.len()
    }

    /// Whether no tag is held.
    pub fn is_empty(&self) -> bool {
        self.tags.is_empty()
    }

    /// Whether the set is sorted with no duplicates. A container built through this API always
    /// is; one loaded from a reflected scene file may not be until [`normalize`](Self::normalize).
    pub fn is_normalized(&self) -> bool {
        self.tags.windows(2).all(|pair| pair[0] < pair[1])
    }

    /// Sorts and de-duplicates the set.
    pub fn normalize(&mut self) {
        self.tags.sort();
        self.tags.dedup();
    }

    /// Adds `tag`; `true` when it was not already held.
    pub fn add(&mut self, tag: Tag) -> bool {
        if self.tags.contains(&tag) {
            return false;
        }
        let at = self.tags.partition_point(|held| *held < tag);
        self.tags.insert(at, tag);
        true
    }

    /// Removes exactly `tag` (its descendants stay); `true` when it was held.
    pub fn remove(&mut self, tag: Tag) -> bool {
        match self.tags.iter().position(|held| *held == tag) {
            Some(at) => {
                self.tags.remove(at);
                true
            }
            None => false,
        }
    }

    /// Removes `tag` and every tag below it; returns how many were removed.
    pub fn remove_branch(&mut self, tag: Tag) -> usize {
        let before = self.tags.len();
        self.tags.retain(|held| !held.matches(tag));
        before - self.tags.len()
    }

    /// Removes every tag.
    pub fn clear(&mut self) {
        self.tags.clear();
    }

    /// Whether `tag` itself is held (no parent or child matching).
    pub fn has_exact(&self, tag: Tag) -> bool {
        self.tags.contains(&tag)
    }

    /// Whether `tag` is held or any held tag is below it: `Combat.Damage` matches a container
    /// holding `Combat.Damage.Fire`. A held parent does **not** match a query for its child.
    pub fn has(&self, tag: Tag) -> bool {
        self.tags.iter().any(|held| held.matches(tag))
    }

    /// [`has`](Self::has) for any of `tags`; `false` for an empty list.
    pub fn has_any(&self, tags: &[Tag]) -> bool {
        tags.iter().any(|tag| self.has(*tag))
    }

    /// [`has`](Self::has) for every one of `tags`; `true` for an empty list.
    pub fn has_all(&self, tags: &[Tag]) -> bool {
        tags.iter().all(|tag| self.has(*tag))
    }

    /// [`has_exact`](Self::has_exact) for any of `tags`; `false` for an empty list.
    pub fn has_any_exact(&self, tags: &[Tag]) -> bool {
        tags.iter().any(|tag| self.has_exact(*tag))
    }

    /// [`has_exact`](Self::has_exact) for every one of `tags`; `true` for an empty list.
    pub fn has_all_exact(&self, tags: &[Tag]) -> bool {
        tags.iter().all(|tag| self.has_exact(*tag))
    }

    /// The tags implied by the explicit ones: every ancestor, sorted, without those that are
    /// themselves explicit.
    pub fn implied_parents(&self) -> Vec<Tag> {
        let mut parents: Vec<Tag> = self
            .tags
            .iter()
            .flat_map(Tag::ancestors)
            .filter(|parent| !self.tags.contains(parent))
            .collect();
        parents.sort();
        parents.dedup();
        parents
    }

    /// The explicit tags plus [`implied_parents`](Self::implied_parents), sorted.
    pub fn expanded(&self) -> Vec<Tag> {
        let mut all = self.implied_parents();
        all.extend(self.tags.iter().copied());
        all.sort();
        all.dedup();
        all
    }

    /// Evaluates a [`TagQuery`] against this set.
    pub fn matches(&self, query: &TagQuery) -> bool {
        query.eval(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(name: &str) -> Tag {
        Tag::new(name).expect("valid tag")
    }

    fn c(names: &[&str]) -> TagContainer {
        TagContainer::from_names(names.iter().copied()).expect("valid names")
    }

    #[test]
    fn parents_match_hierarchical_queries_only_downward() {
        let held = c(&["Combat.Damage.Fire"]);
        assert!(held.has(t("Combat.Damage.Fire")));
        assert!(held.has(t("Combat.Damage")));
        assert!(held.has(t("Combat")));
        assert!(!held.has(t("Combat.Damage.Ice")));
        assert!(!held.has(t("Combat.DamageType")));
        // The reverse direction does not match: holding the parent does not hold the child.
        let parent = c(&["Combat.Damage"]);
        assert!(!parent.has(t("Combat.Damage.Fire")));
    }

    #[test]
    fn exact_queries_ignore_hierarchy() {
        let held = c(&["Combat.Damage.Fire"]);
        assert!(held.has_exact(t("Combat.Damage.Fire")));
        assert!(!held.has_exact(t("Combat.Damage")));
        assert!(!held.has_exact(t("Combat")));
    }

    #[test]
    fn any_and_all_variants() {
        let held = c(&["Combat.Damage.Fire", "Status.Burning"]);
        let fire = t("Combat.Damage");
        let burning = t("Status.Burning");
        let ice = t("Combat.Damage.Ice");
        assert!(held.has_any(&[ice, fire]));
        assert!(!held.has_any(&[ice]));
        assert!(held.has_all(&[fire, burning]));
        assert!(!held.has_all(&[fire, ice]));
        assert!(!held.has_any_exact(&[fire]));
        assert!(held.has_any_exact(&[fire, burning]));
        assert!(!held.has_all_exact(&[fire, burning]));
        assert!(held.has_all_exact(&[burning]));
        // Empty lists: nothing to satisfy for `all`, nothing to hit for `any`.
        assert!(held.has_all(&[]) && held.has_all_exact(&[]));
        assert!(!held.has_any(&[]) && !held.has_any_exact(&[]));
    }

    #[test]
    fn add_remove_keep_the_set_sorted_and_unique() {
        let mut set = TagContainer::new();
        assert!(set.add(t("Zeta.One")));
        assert!(set.add(t("Alpha.Two")));
        assert!(!set.add(t("Alpha.Two")));
        assert!(set.add(t("Alpha.One")));
        assert_eq!(set.names(), ["Alpha.One", "Alpha.Two", "Zeta.One"]);
        assert!(set.is_normalized());
        assert!(set.remove(t("Alpha.Two")));
        assert!(!set.remove(t("Alpha.Two")));
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn removing_a_parent_keeps_children_and_branch_removal_drops_them() {
        let mut set = c(&["Combat.Damage", "Combat.Damage.Fire", "Combat.Heal", "Other"]);
        assert!(set.remove(t("Combat.Damage")));
        assert!(set.has_exact(t("Combat.Damage.Fire")));
        assert!(set.add(t("Combat.Damage")));
        assert_eq!(set.remove_branch(t("Combat.Damage")), 2);
        assert_eq!(set.names(), ["Combat.Heal", "Other"]);
    }

    #[test]
    fn parent_bookkeeping() {
        let set = c(&["Combat.Damage.Fire", "Combat.Damage", "Status.Burning.Deep"]);
        let implied: Vec<&str> = set.implied_parents().iter().map(Tag::name).collect();
        assert_eq!(implied, ["Combat", "Status", "Status.Burning"]);
        let expanded: Vec<&str> = set.expanded().iter().map(Tag::name).collect();
        assert_eq!(
            expanded,
            [
                "Combat",
                "Combat.Damage",
                "Combat.Damage.Fire",
                "Status",
                "Status.Burning",
                "Status.Burning.Deep"
            ]
        );
    }

    #[test]
    fn iteration_order_is_by_name_whatever_the_insertion_order() {
        let names = ["M.C", "A.Z", "A.B", "Z", "M"];
        let forward = c(&names);
        let mut reversed = names;
        reversed.reverse();
        let backward = c(&reversed);
        assert_eq!(forward, backward);
        assert_eq!(forward.names(), ["A.B", "A.Z", "M", "M.C", "Z"]);
    }

    #[test]
    fn ron_round_trip_uses_names_and_normalizes() {
        let set = c(&["Status.Burning", "Combat.Damage.Fire"]);
        let text = ron::to_string(&set).expect("ron");
        assert!(
            text.contains(r#""Combat.Damage.Fire","Status.Burning""#),
            "{text}"
        );
        let back: TagContainer = ron::from_str(&text).expect("parse");
        assert_eq!(back, set);
        // Hand-written order and duplicates are fixed on load.
        let messy: TagContainer = ron::from_str(r#"(tags: ["B", "A", "B"])"#).expect("parse");
        assert_eq!(messy.names(), ["A", "B"]);
        assert!(ron::from_str::<TagContainer>(r#"(tags: ["Bad Name"])"#).is_err());
    }

    #[test]
    fn normalize_repairs_an_unsorted_set() {
        let mut set = TagContainer {
            tags: vec![t("B.Norm"), t("A.Norm"), t("B.Norm")],
        };
        assert!(!set.is_normalized());
        set.normalize();
        assert!(set.is_normalized());
        assert_eq!(set.names(), ["A.Norm", "B.Norm"]);
    }
}
