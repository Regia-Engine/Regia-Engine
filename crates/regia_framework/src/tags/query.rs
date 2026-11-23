// SPDX-License-Identifier: MPL-2.0
//! [`TagQuery`]: a small boolean expression over a [`TagContainer`], with a RON form.

use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use super::container::TagContainer;
use super::name::{Tag, TagError};

/// Deepest nesting a parsed query may have.
pub const MAX_QUERY_DEPTH: usize = 16;

/// Most nodes (leaves and combinators) a parsed query may have.
pub const MAX_QUERY_NODES: usize = 1024;

/// A boolean expression over the tags a [`TagContainer`] holds.
///
/// Leaves test one tag; combinators nest. Truth tables (`n` children, `k` of them true):
///
/// | Node | Result |
/// |------|--------|
/// | `Has(t)` | the container holds `t` or a tag below it |
/// | `HasExact(t)` | the container holds `t` itself |
/// | `AllOf(children)` | `k == n` (true for no children) |
/// | `AnyOf(children)` | `k >= 1` (false for no children) |
/// | `NoneOf(children)` | `k == 0` (true for no children) |
///
/// The RON form is the enum itself with tags as name strings:
///
/// ```
/// use regia_framework::tags::{TagContainer, TagQuery};
///
/// let query = TagQuery::from_ron(
///     r#"AllOf([Has("Combat.Damage"), NoneOf([HasExact("Status.Immune")])])"#,
/// ).expect("valid query");
/// let burning = TagContainer::from_names(["Combat.Damage.Fire"]).expect("names");
/// assert!(burning.matches(&query));
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TagQuery {
    /// The container holds this tag or one below it.
    Has(Tag),
    /// The container holds exactly this tag.
    HasExact(Tag),
    /// Every child matches.
    AllOf(Vec<TagQuery>),
    /// At least one child matches.
    AnyOf(Vec<TagQuery>),
    /// No child matches.
    NoneOf(Vec<TagQuery>),
}

/// A set of tags a [`TagQuery`] can be asked about, without building a [`TagContainer`] for it (a
/// per-step check over two sources allocates nothing this way).
pub trait TagLookup {
    /// The set holds `tag` or one below it.
    fn has_tag(&self, tag: Tag) -> bool;
    /// The set holds exactly `tag`.
    fn has_tag_exact(&self, tag: Tag) -> bool;
}

impl TagLookup for TagContainer {
    fn has_tag(&self, tag: Tag) -> bool {
        self.has(tag)
    }

    fn has_tag_exact(&self, tag: Tag) -> bool {
        self.has_exact(tag)
    }
}

impl TagQuery {
    /// Evaluates against `container`.
    pub fn eval(&self, container: &TagContainer) -> bool {
        self.eval_on(container)
    }

    /// Evaluates against any tag set.
    pub fn eval_on(&self, set: &impl TagLookup) -> bool {
        match self {
            TagQuery::Has(tag) => set.has_tag(*tag),
            TagQuery::HasExact(tag) => set.has_tag_exact(*tag),
            TagQuery::AllOf(children) => children.iter().all(|child| child.eval_on(set)),
            TagQuery::AnyOf(children) => children.iter().any(|child| child.eval_on(set)),
            TagQuery::NoneOf(children) => !children.iter().any(|child| child.eval_on(set)),
        }
    }

    /// Every tag the query mentions, sorted without duplicates (for validation against a tag table).
    pub fn tags(&self) -> Vec<Tag> {
        self.tags_small().into_vec()
    }

    /// [`TagQuery::tags`] into a stack buffer. Queries of eight tags or fewer allocate nothing.
    pub(crate) fn tags_small(&self) -> SmallVec<[Tag; 8]> {
        let mut out = SmallVec::new();
        self.collect(&mut out);
        out.sort();
        out.dedup();
        out
    }

    fn collect(&self, out: &mut SmallVec<[Tag; 8]>) {
        match self {
            TagQuery::Has(tag) | TagQuery::HasExact(tag) => out.push(*tag),
            TagQuery::AllOf(children) | TagQuery::AnyOf(children) | TagQuery::NoneOf(children) => {
                for child in children {
                    child.collect(out);
                }
            }
        }
    }

    /// Checks the nesting depth and node count against [`MAX_QUERY_DEPTH`] and [`MAX_QUERY_NODES`].
    pub fn validate(&self) -> Result<(), TagError> {
        let mut nodes = 0usize;
        self.check(1, &mut nodes)
    }

    fn check(&self, depth: usize, nodes: &mut usize) -> Result<(), TagError> {
        *nodes += 1;
        if depth > MAX_QUERY_DEPTH {
            return Err(TagError::QueryTooLarge(format!(
                "nested deeper than {MAX_QUERY_DEPTH} levels"
            )));
        }
        if *nodes > MAX_QUERY_NODES {
            return Err(TagError::QueryTooLarge(format!(
                "more than {MAX_QUERY_NODES} nodes"
            )));
        }
        match self {
            TagQuery::Has(_) | TagQuery::HasExact(_) => Ok(()),
            TagQuery::AllOf(children) | TagQuery::AnyOf(children) | TagQuery::NoneOf(children) => children
                .iter()
                .try_for_each(|child| child.check(depth + 1, nodes)),
        }
    }

    /// Parses the RON form and checks the size limits.
    pub fn from_ron(text: &str) -> Result<Self, TagError> {
        let query: Self = ron::from_str(text).map_err(|e| TagError::Parse(e.to_string()))?;
        query.validate()?;
        Ok(query)
    }

    /// Writes the RON form.
    pub fn to_ron(&self) -> Result<String, TagError> {
        ron::to_string(self).map_err(|e| TagError::Parse(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(name: &str) -> Tag {
        Tag::new(name).expect("valid tag")
    }

    fn held(names: &[&str]) -> TagContainer {
        TagContainer::from_names(names.iter().copied()).expect("names")
    }

    fn has(name: &str) -> TagQuery {
        TagQuery::Has(t(name))
    }

    /// Truth tables of the three combinators over children that are fixed true or false.
    #[test]
    fn combinator_truth_tables() {
        let container = held(&["Query.On"]);
        let yes = || has("Query.On");
        let no = || has("Query.Off");
        // (children, all, any, none)
        let cases: Vec<(Vec<TagQuery>, bool, bool, bool)> = vec![
            (vec![], true, false, true),
            (vec![yes()], true, true, false),
            (vec![no()], false, false, true),
            (vec![yes(), yes()], true, true, false),
            (vec![yes(), no()], false, true, false),
            (vec![no(), no()], false, false, true),
        ];
        for (children, all, any, none) in cases {
            assert_eq!(
                TagQuery::AllOf(children.clone()).eval(&container),
                all,
                "{children:?}"
            );
            assert_eq!(
                TagQuery::AnyOf(children.clone()).eval(&container),
                any,
                "{children:?}"
            );
            assert_eq!(
                TagQuery::NoneOf(children.clone()).eval(&container),
                none,
                "{children:?}"
            );
        }
    }

    #[test]
    fn leaves_follow_the_container_rules() {
        let container = held(&["Combat.Damage.Fire"]);
        assert!(TagQuery::Has(t("Combat.Damage")).eval(&container));
        assert!(!TagQuery::HasExact(t("Combat.Damage")).eval(&container));
        assert!(TagQuery::HasExact(t("Combat.Damage.Fire")).eval(&container));
        assert!(!TagQuery::Has(t("Combat.Damage.Fire.Deep")).eval(&container));
    }

    #[test]
    fn nested_expression() {
        // Burning damage, unless immune, or anything stunned.
        let query = TagQuery::AnyOf(vec![
            TagQuery::AllOf(vec![
                has("Nest.Damage"),
                TagQuery::NoneOf(vec![has("Nest.Immune")]),
            ]),
            has("Nest.Stunned"),
        ]);
        assert!(query.eval(&held(&["Nest.Damage.Fire"])));
        assert!(!query.eval(&held(&["Nest.Damage.Fire", "Nest.Immune.Fire"])));
        assert!(query.eval(&held(&["Nest.Damage.Fire", "Nest.Immune", "Nest.Stunned"])));
        assert!(!query.eval(&held(&[])));
    }

    #[test]
    fn ron_round_trip() {
        let query = TagQuery::AllOf(vec![
            has("Combat.Damage"),
            TagQuery::NoneOf(vec![TagQuery::HasExact(t("Status.Immune"))]),
            TagQuery::AnyOf(vec![has("A"), has("B")]),
        ]);
        let text = query.to_ron().expect("ron");
        assert_eq!(TagQuery::from_ron(&text).expect("parse"), query);
        let hand = TagQuery::from_ron(
            r#"AllOf([Has("Combat.Damage"), NoneOf([HasExact("Status.Immune")]), AnyOf([Has("A"), Has("B")])])"#,
        )
        .expect("hand written");
        assert_eq!(hand, query);
    }

    #[test]
    fn json_round_trip_is_the_agent_form() {
        let query = TagQuery::AnyOf(vec![has("Json.A"), TagQuery::NoneOf(vec![has("Json.B")])]);
        let value = serde_json::to_value(&query).expect("json");
        assert_eq!(
            value,
            serde_json::json!({ "AnyOf": [{ "Has": "Json.A" }, { "NoneOf": [{ "Has": "Json.B" }] }] })
        );
        assert_eq!(serde_json::from_value::<TagQuery>(value).expect("parse"), query);
    }

    #[test]
    fn bad_input_is_a_typed_error() {
        assert!(matches!(
            TagQuery::from_ron(r#"Has("Bad Name")"#),
            Err(TagError::Parse(_))
        ));
        assert!(matches!(TagQuery::from_ron("Wat([])"), Err(TagError::Parse(_))));
        let mut deep = has("Deep.Leaf");
        for _ in 0..MAX_QUERY_DEPTH {
            deep = TagQuery::NoneOf(vec![deep]);
        }
        assert!(matches!(deep.validate(), Err(TagError::QueryTooLarge(_))));
        let wide = TagQuery::AnyOf(vec![has("Wide.Leaf"); MAX_QUERY_NODES]);
        assert!(matches!(wide.validate(), Err(TagError::QueryTooLarge(_))));
        assert!(
            TagQuery::AnyOf(vec![has("Wide.Leaf"); MAX_QUERY_NODES - 1])
                .validate()
                .is_ok()
        );
    }

    #[test]
    fn tags_lists_each_mentioned_tag_once() {
        let query = TagQuery::AllOf(vec![
            has("Mention.B"),
            has("Mention.A"),
            TagQuery::NoneOf(vec![has("Mention.B")]),
        ]);
        let names: Vec<&str> = query.tags().iter().map(Tag::name).collect();
        assert_eq!(names, ["Mention.A", "Mention.B"]);
    }
}
