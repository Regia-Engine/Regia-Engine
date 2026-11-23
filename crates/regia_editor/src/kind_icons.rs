// SPDX-License-Identifier: MPL-2.0
//! Outliner row icons: the entity's kind (icon tinted with the kind's color), else its derived role,
//! and the unclassified marker (`docs/specs/101-entity-kinds-and-project-map.md` § 4).
//!
//! The rule lives in `regia_scene::kind` (runtime); this module only maps icon names to glyphs of the
//! editor's default fonts and builds the row's tooltip. The viewport's gizmo icons
//! (`crate::overlays::icon_of`) are unchanged: rows 1-6 of the role table are that order.

use regia_ecs::prelude::*;
use regia_scene::kind::roles::{Role, classify_entity};
use regia_scene::kind::{UNCATEGORIZED, kind_of};

/// The category unkinded entities are grouped under.
pub const NO_KIND: &str = "No kind";

/// The icon names of spec 101 § 4.3 and their glyphs (kind icons, then role names).
pub const ICONS: &[(&str, &str)] = &[
    ("crate", "\u{1f4e6}"),
    ("box", "\u{2610}"),
    ("enemy", "\u{1f47e}"),
    ("person", "\u{1f464}"),
    ("door", "\u{1f6aa}"),
    ("key", "\u{1f511}"),
    ("coin", "\u{1f4b0}"),
    ("chest", "\u{1f4bc}"),
    ("flag", "\u{1f6a9}"),
    ("spawn", "\u{2733}"),
    ("pickup", "\u{270b}"),
    ("weapon", "\u{1f5e1}"),
    ("vehicle", "\u{1f697}"),
    ("trigger", "\u{2b1a}"),
    ("light", "\u{1f4a1}"),
    ("camera", "\u{1f3a5}"),
    ("sound", "\u{1f50a}"),
    ("tree", "\u{1f332}"),
    ("rock", "\u{1f5ff}"),
    ("water", "\u{1f30a}"),
    ("fire", "\u{1f525}"),
    ("star", "\u{2605}"),
    ("gear", "\u{2699}"),
    ("kind", "\u{25c6}"),
    ("sun", "\u{2600}"),
    ("spot", "\u{1f526}"),
    ("point", "\u{1f4a1}"),
    ("decal", "\u{1f5bc}"),
    ("audio", "\u{1f50a}"),
    ("character", "\u{1f6b6}"),
    ("body", "\u{2699}"),
    ("mesh", "\u{25a0}"),
    ("collider", "\u{25a3}"),
    ("terrain", "\u{1f5fb}"),
    ("probe", "\u{25ce}"),
    ("spline", "\u{3030}"),
    ("volume", "\u{2b1c}"),
    ("ui", "\u{1f5d6}"),
];

/// The glyph of an icon name; an unknown name gets the generic kind glyph.
#[must_use]
pub fn glyph(icon: &str) -> &'static str {
    ICONS
        .iter()
        .find(|(name, _)| *name == icon)
        .map_or("\u{25c6}", |(_, glyph)| glyph)
}

/// What an outliner row shows before the name.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RowIcon {
    /// The glyph, or empty for an entity with no kind and no role (a group node).
    pub glyph: &'static str,
    /// The kind's color (`None`: neutral text color).
    pub color: Option<[u8; 3]>,
    /// The kind's name.
    pub kind: Option<String>,
    /// The derived role (also set for kinded entities).
    pub role: Option<Role>,
    /// No kind and two or more role families: shows the `?` marker.
    pub unclassified: bool,
    /// The grouping category: the kind's category, [`UNCATEGORIZED`], or [`NO_KIND`].
    pub category: String,
    /// The hover text.
    pub tooltip: String,
}

/// The row icon of `entity`.
#[must_use]
pub fn row_icon(world: &World, entity: Entity) -> RowIcon {
    let classification = classify_entity(world, entity);
    let families: Vec<&str> = classification.families.iter().map(|f| f.name()).collect();
    if let Some(kind) = kind_of(world, entity) {
        let (glyph, color, category, mut tooltip) = match &kind.info {
            Some(info) => (
                self::glyph(&info.icon),
                Some(info.color),
                info.category_name().to_string(),
                if info.description.is_empty() {
                    format!("kind: {}", info.name)
                } else {
                    format!("kind: {}\n{}", info.name, info.description)
                },
            ),
            None => (
                self::glyph("kind"),
                None,
                UNCATEGORIZED.to_string(),
                format!(
                    "kind: {} (not declared: no .kind.ron or code kind has this name)",
                    kind.name
                ),
            ),
        };
        if kind.via_prefab {
            tooltip.push_str("\n(from its prefab)");
        }
        if !kind.conflicts.is_empty() {
            tooltip.push_str(&format!("\nalso carries: {}", kind.conflicts.join(", ")));
        }
        return RowIcon {
            glyph,
            color,
            kind: Some(kind.name),
            role: classification.role,
            unclassified: false,
            category,
            tooltip,
        };
    }
    let unclassified = classification.is_multi_family();
    let (glyph, tooltip) = match classification.role {
        Some(role) if unclassified => (
            self::glyph(role.name()),
            format!(
                "unclassified: {} ({})\ntwo roles on one entity, consider a child",
                role.name(),
                families.join(" + ")
            ),
        ),
        Some(role) => (self::glyph(role.name()), role.name().to_string()),
        None => ("", String::new()),
    };
    RowIcon {
        glyph,
        color: None,
        kind: None,
        role: classification.role,
        unclassified,
        category: NO_KIND.to_string(),
        tooltip,
    }
}

/// A search's kind filter (`kind:<name>`, `kind:none`, `kind:?`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KindFilter {
    /// The kind of that name (lowercase, compared case-insensitively).
    Name(String),
    /// Entities without a kind.
    None,
    /// Unclassified entities.
    Unclassified,
}

impl KindFilter {
    /// Whether a row matches.
    #[must_use]
    pub fn matches(&self, icon: &RowIcon) -> bool {
        match self {
            Self::Name(name) => icon.kind.as_deref().is_some_and(|k| k.to_lowercase() == *name),
            Self::None => icon.kind.is_none(),
            Self::Unclassified => icon.unclassified,
        }
    }
}

/// The entity's search facts for the Outliner's `type:` and `tag:` terms (spec 19 § 2): its
/// components' short type names (a generic one by its outer type) and its tags, all lowercase.
#[must_use]
pub fn row_terms(world: &World, entity: Entity) -> (Vec<String>, Vec<String>) {
    let mut components = Vec::new();
    if let Ok(infos) = world.inspect_entity(entity) {
        for (_, info) in infos {
            let path = info.name().to_string();
            // A generic type's term is its outer type: `a::Foo<b::Bar>` is `foo`.
            let outer = path.split('<').next().unwrap_or(path.as_str());
            let short = outer.rsplit("::").next().unwrap_or(outer);
            components.push(short.to_lowercase());
        }
    }
    let tags = world
        .get::<regia_scene::components::Tags>(entity)
        .map(|tags| tags.0.iter().map(|t| t.as_str().to_lowercase()).collect())
        .unwrap_or_default();
    (components, tags)
}

/// Splits a search into its kind filter and the name part: `kind:Crate lid` → (`Name("crate")`,
/// `"lid"`). Lowercases both.
///
/// Component-type and tag terms live in [`parse_search`], which supersedes this shorthand.
#[must_use]
pub fn parse_query(query: &str) -> (Option<KindFilter>, String) {
    let parsed = parse_search(query);
    (parsed.kind, parsed.name)
}

/// An Outliner search split into its terms (spec 19 § 2, spec 101 § 4.2): the kind filter, the
/// component-type terms, the tag terms and the name part. Every term is lowercase; the terms
/// combine with AND.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SearchQuery {
    /// The `kind:` filter.
    pub kind: Option<KindFilter>,
    /// Every `type:<Component>` term (short component names, e.g. `meshsource`).
    pub types: Vec<String>,
    /// Every `tag:<name>` term (the `Tags` component's values).
    pub tags: Vec<String>,
    /// The remaining words, joined: the name filter.
    pub name: String,
}

impl SearchQuery {
    /// Whether no term is set (an empty or blank search).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.kind.is_none() && self.types.is_empty() && self.tags.is_empty() && self.name.is_empty()
    }
}

/// Splits a search into its terms: `kind:Crate type:MeshSource tag:loot lid` → the kind
/// `Name("crate")`, `["meshsource"]`, `["loot"]` and `"lid"`. Lowercases everything; repeated
/// terms keep every value; bare `kind:`, `type:` and `tag:` are ignored.
#[must_use]
pub fn parse_search(query: &str) -> SearchQuery {
    let mut parsed = SearchQuery::default();
    let mut rest = Vec::new();
    for token in query.split_whitespace() {
        let token = token.to_lowercase();
        if let Some(kind) = token.strip_prefix("kind:") {
            parsed.kind = match kind {
                "" => parsed.kind,
                "none" => Some(KindFilter::None),
                "?" | "unclassified" => Some(KindFilter::Unclassified),
                name => Some(KindFilter::Name(name.to_string())),
            };
        } else if let Some(ty) = token.strip_prefix("type:") {
            if !ty.is_empty() {
                parsed.types.push(ty.to_string());
            }
        } else if let Some(tag) = token.strip_prefix("tag:") {
            if !tag.is_empty() {
                parsed.tags.push(tag.to_string());
            }
        } else {
            rest.push(token);
        }
    }
    parsed.name = rest.join(" ");
    parsed
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_scene::kind::roles::Role;

    #[test]
    fn every_role_and_spec_icon_has_a_glyph() {
        for role in Role::PRIORITY {
            assert!(
                ICONS.iter().any(|(name, _)| *name == role.name()),
                "{} has no glyph",
                role.name()
            );
        }
        for name in [
            "crate", "box", "enemy", "person", "door", "key", "coin", "chest", "flag", "spawn", "pickup",
            "weapon", "vehicle", "trigger", "light", "camera", "sound", "tree", "rock", "water", "fire",
            "star", "gear", "kind",
        ] {
            assert!(ICONS.iter().any(|(n, _)| *n == name), "{name}");
        }
        assert_eq!(glyph("no-such-icon"), glyph("kind"));
    }

    #[test]
    fn queries_split_into_kind_and_name() {
        assert_eq!(
            parse_query("kind:Crate  Lid"),
            (Some(KindFilter::Name("crate".into())), "lid".into())
        );
        assert_eq!(parse_query("kind:none"), (Some(KindFilter::None), String::new()));
        assert_eq!(
            parse_query("kind:?"),
            (Some(KindFilter::Unclassified), String::new())
        );
        assert_eq!(parse_query("Lamp"), (None, "lamp".into()));
    }

    #[test]
    fn queries_split_component_type_and_tag_tokens() {
        use super::SearchQuery;
        let q = |kind: Option<KindFilter>, types: &[&str], tags: &[&str], name: &str| SearchQuery {
            kind,
            types: types.iter().map(|s| s.to_string()).collect(),
            tags: tags.iter().map(|s| s.to_string()).collect(),
            name: name.to_string(),
        };
        assert_eq!(parse_search("type:MeshSource"), q(None, &["meshsource"], &[], ""));
        assert_eq!(parse_search("tag:Loot"), q(None, &[], &["loot"], ""));
        // The tokens combine with each other, with `kind:` and with the name filter.
        assert_eq!(
            parse_search("kind:Crate type:MeshSource tag:loot lid"),
            q(
                Some(KindFilter::Name("crate".into())),
                &["meshsource"],
                &["loot"],
                "lid"
            )
        );
        // Repeated tokens keep every value; bare `type:` and `tag:` are ignored.
        assert_eq!(parse_search("type: type:Tags tag:"), q(None, &["tags"], &[], ""));
    }

    /// w10 (sprint-w9-rev-v06-lanes § 2 finding 6): a generic component's `type:` term is its
    /// outer type's short name (`Wrapper<…::Marker>` is `wrapper`, not `marker>`), so
    /// `type:wrapper` finds it; a plain component keeps its short name.
    #[test]
    fn a_generic_component_matches_type_by_its_outer_name() {
        #[derive(Component)]
        struct Wrapper<T: Send + Sync + 'static>(T);
        #[derive(Debug, PartialEq)]
        struct Marker;
        #[derive(Component)]
        struct Plain;

        let mut world = World::new();
        let entity = world.spawn((Wrapper(Marker), Wrapper(0_u32), Plain)).id();
        let (components, _) = row_terms(&world, entity);
        assert_eq!(world.get::<Wrapper<u32>>(entity).map(|w| w.0), Some(0));
        assert_eq!(world.get::<Wrapper<Marker>>(entity).map(|w| &w.0), Some(&Marker));
        assert!(components.contains(&"plain".to_string()), "{components:?}");
        assert_eq!(
            components.iter().filter(|c| *c == "wrapper").count(),
            2,
            "both instances of the generic: {components:?}"
        );
        assert!(
            components.iter().all(|c| !c.contains('<') && !c.contains('>')),
            "no generic arguments in a term: {components:?}"
        );
        let query = parse_search("type:wrapper");
        assert!(query.types.iter().all(|t| components.contains(t)), "{query:?}");
    }
}
