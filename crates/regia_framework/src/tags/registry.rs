// SPDX-License-Identifier: MPL-2.0
//! The tag table (`*.tags.ron`) and the [`TagRegistry`] built from it.
//!
//! A game lists the tags it uses in a table so a typo in an asset is an error, not a silent
//! never-matching tag. Declaring `Combat.Damage.Fire` also declares its parents (`Combat.Damage`
//! and `Combat`) as *implicit* tags; a parent may also be declared on its own to give it a
//! description.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use regia_app::prelude::*;
use regia_core::version;
use serde::{Deserialize, Serialize};

use super::container::TagContainer;
use super::name::{Tag, TagError, validate_tag_name};

/// Current `.tags.ron` version.
pub const TAG_TABLE_FORMAT_VERSION: u32 = 1;

/// One declared tag in a [`TagTable`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagDecl {
    /// The full dotted name.
    pub name: String,
    /// What the tag means, for designers and agents.
    #[serde(default)]
    pub description: String,
}

/// The `.tags.ron` document: the tags a game declares.
///
/// ```ron
/// (
///     version: 1,
///     tags: [
///         // Damage kinds.
///         (name: "Combat.Damage.Fire", description: "Fire damage"),
///         (name: "Status.Burning"),
///     ],
/// )
/// ```
///
/// RON `//` comments are read and dropped; [`to_ron`](Self::to_ron) writes none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagTable {
    /// Format version, [`TAG_TABLE_FORMAT_VERSION`].
    pub version: u32,
    /// What the table is for.
    #[serde(default)]
    pub description: String,
    /// The declared tags.
    pub tags: Vec<TagDecl>,
}

impl Default for TagTable {
    fn default() -> Self {
        Self {
            version: TAG_TABLE_FORMAT_VERSION,
            description: String::new(),
            tags: Vec::new(),
        }
    }
}

impl TagTable {
    /// Parses a `.tags.ron`: version check, then shape. Names are not checked here, so
    /// [`problems`](Self::problems) can report every bad entry at once.
    pub fn from_ron(text: &str) -> Result<Self, TagError> {
        let found = version::peek_ron_version(text).map_err(TagError::Parse)?;
        if found > TAG_TABLE_FORMAT_VERSION {
            return Err(TagError::Version {
                found,
                expected: TAG_TABLE_FORMAT_VERSION,
            });
        }
        ron::from_str(text).map_err(|e| TagError::Parse(e.to_string()))
    }

    /// Writes the table as RON with a trailing newline.
    pub fn to_ron(&self) -> Result<String, TagError> {
        let mut text = ron::ser::to_string_pretty(self, regia_core::ron_pretty())
            .map_err(|e| TagError::Parse(e.to_string()))?;
        text.push('\n');
        Ok(text)
    }

    /// Every problem in the table, in file order: invalid names, duplicate names, ids that
    /// collide. An empty list means [`TagRegistry::from_table`] succeeds.
    pub fn problems(&self) -> Vec<TagError> {
        let mut problems = Vec::new();
        let mut seen = BTreeSet::new();
        for decl in &self.tags {
            if let Err(error) = validate_tag_name(&decl.name) {
                problems.push(error);
            } else if !seen.insert(decl.name.as_str()) {
                problems.push(TagError::Duplicate {
                    name: decl.name.clone(),
                });
            } else if let Err(error) = Tag::new(&decl.name) {
                problems.push(error);
            }
        }
        problems
    }
}

/// The `.tags.ron` migration chain: v1 is the first version, so there are no steps.
#[must_use]
pub fn tag_table_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// The serde shape fingerprint of `.tags.ron` and the [`TagQuery`](super::TagQuery) form that
/// assets embed (hashed by `cargo xtask check-formats`): the field list of each struct, then the
/// query's variant spellings.
#[must_use]
pub fn tag_table_schema_fingerprint() -> String {
    use regia_core::schema::{serde_field_names, serde_variant_names};
    regia_core::schema::join_field_lists(&[
        serde_field_names::<TagTable>(),
        serde_field_names::<TagDecl>(),
        serde_variant_names::<super::TagQuery>(),
    ])
}

/// One entry of a [`TagRegistry`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagEntry {
    /// The tag.
    pub tag: Tag,
    /// Its description (empty for an undescribed or implicit tag).
    pub description: String,
    /// Whether it is declared only as the parent of a declared tag.
    pub implicit: bool,
}

/// A usage of an undeclared or invalid tag found by a validation pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagDiagnostic {
    /// Where the tag was used (an asset path, an entity, a query).
    pub source: String,
    /// The name as written.
    pub name: String,
    /// What is wrong with it.
    pub error: TagError,
}

/// The declared tags of a game, as an ECS resource.
///
/// Iteration is sorted by name. Built from a [`TagTable`] or with [`declare`](Self::declare).
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct TagRegistry {
    entries: BTreeMap<&'static str, TagEntry>,
}

impl TagRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds a registry from a table; the first problem of [`TagTable::problems`] is the error.
    pub fn from_table(table: &TagTable) -> Result<Self, TagError> {
        if let Some(problem) = table.problems().into_iter().next() {
            return Err(problem);
        }
        let mut registry = Self::new();
        for decl in &table.tags {
            registry.declare(&decl.name, &decl.description)?;
        }
        Ok(registry)
    }

    /// Reads and builds a registry from a `.tags.ron` file.
    pub fn load(path: &Path) -> Result<Self, TagError> {
        let text = std::fs::read_to_string(path).map_err(|e| TagError::Io {
            path: path.display().to_string(),
            message: e.to_string(),
        })?;
        Self::from_table(&TagTable::from_ron(&text)?)
    }

    /// Declares `name` (and its parents, implicitly). Declaring a name that is already declared
    /// explicitly is [`TagError::Duplicate`]; declaring one that was only implied by a child
    /// upgrades it and sets its description.
    pub fn declare(&mut self, name: &str, description: &str) -> Result<Tag, TagError> {
        let tag = Tag::new(name)?;
        if self.entries.get(tag.name()).is_some_and(|entry| !entry.implicit) {
            return Err(TagError::Duplicate {
                name: name.to_owned(),
            });
        }
        for parent in tag.ancestors() {
            self.entries.entry(parent.name()).or_insert(TagEntry {
                tag: parent,
                description: String::new(),
                implicit: true,
            });
        }
        self.entries.insert(
            tag.name(),
            TagEntry {
                tag,
                description: description.to_owned(),
                implicit: false,
            },
        );
        Ok(tag)
    }

    /// The number of declared tags, implicit ones included.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is declared.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The entries sorted by name.
    pub fn iter(&self) -> impl Iterator<Item = &TagEntry> {
        self.entries.values()
    }

    /// The declared tag named `name`.
    pub fn get(&self, name: &str) -> Option<Tag> {
        self.entries.get(name).map(|entry| entry.tag)
    }

    /// The entry of a declared tag.
    pub fn entry(&self, tag: Tag) -> Option<&TagEntry> {
        self.entries.get(tag.name())
    }

    /// Whether `tag` is declared (explicitly or as a parent).
    pub fn contains(&self, tag: Tag) -> bool {
        self.entries.contains_key(tag.name())
    }

    /// The declared tag named `name`: an error for an invalid or undeclared name.
    pub fn resolve(&self, name: &str) -> Result<Tag, TagError> {
        validate_tag_name(name)?;
        self.get(name).ok_or_else(|| TagError::Undeclared {
            name: name.to_owned(),
        })
    }

    /// The declared tags directly below `tag`, sorted.
    pub fn children(&self, tag: Tag) -> Vec<Tag> {
        self.entries
            .values()
            .filter(|entry| entry.tag.parent() == Some(tag))
            .map(|entry| entry.tag)
            .collect()
    }

    /// The explicitly declared tags as a table, sorted by name (implicit parents are left out;
    /// loading the table restores them).
    pub fn to_table(&self) -> TagTable {
        TagTable {
            version: TAG_TABLE_FORMAT_VERSION,
            description: String::new(),
            tags: self
                .entries
                .values()
                .filter(|entry| !entry.implicit)
                .map(|entry| TagDecl {
                    name: entry.tag.name().to_owned(),
                    description: entry.description.clone(),
                })
                .collect(),
        }
    }

    /// Checks names an asset uses: each must be a valid, declared tag. `source` names the asset
    /// in the diagnostics. Every bad name is reported, in input order.
    pub fn check_names<'a>(
        &self,
        source: &str,
        names: impl IntoIterator<Item = &'a str>,
    ) -> Vec<TagDiagnostic> {
        names
            .into_iter()
            .filter_map(|name| {
                self.resolve(name).err().map(|error| TagDiagnostic {
                    source: source.to_owned(),
                    name: name.to_owned(),
                    error,
                })
            })
            .collect()
    }

    /// Checks the tags of a container: those not declared, in the container's order.
    pub fn check_container(&self, source: &str, container: &TagContainer) -> Vec<TagDiagnostic> {
        self.check_names(source, container.iter().map(|tag| tag.name()))
    }

    /// Checks every tag a query mentions.
    pub fn check_query(&self, source: &str, query: &super::TagQuery) -> Vec<TagDiagnostic> {
        self.check_names(source, query.tags().iter().map(|tag| tag.name()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: &str = r#"(
    version: 1,
    description: "Test tags",
    tags: [
        // Damage kinds.
        (name: "Reg.Combat.Damage.Fire", description: "Fire damage"),
        (name: "Reg.Combat.Damage.Ice"),
        (name: "Reg.Combat", description: "All combat"),
        (name: "Reg.Status.Burning"),
    ],
)"#;

    fn registry() -> TagRegistry {
        TagRegistry::from_table(&TagTable::from_ron(TABLE).expect("table")).expect("registry")
    }

    #[test]
    fn table_builds_a_registry_with_implicit_parents() {
        let registry = registry();
        let names: Vec<&str> = registry.iter().map(|e| e.tag.name()).collect();
        assert_eq!(
            names,
            [
                "Reg",
                "Reg.Combat",
                "Reg.Combat.Damage",
                "Reg.Combat.Damage.Fire",
                "Reg.Combat.Damage.Ice",
                "Reg.Status",
                "Reg.Status.Burning"
            ]
        );
        let combat = registry.get("Reg.Combat").expect("declared");
        assert_eq!(
            registry.entry(combat).map(|e| e.description.as_str()),
            Some("All combat")
        );
        assert!(!registry.entry(combat).expect("entry").implicit);
        assert!(
            registry
                .entry(registry.get("Reg").expect("root"))
                .expect("entry")
                .implicit
        );
        let damage = registry.get("Reg.Combat.Damage").expect("implicit");
        let kids: Vec<&str> = registry.children(damage).iter().map(|t| t.name()).collect();
        assert_eq!(kids, ["Reg.Combat.Damage.Fire", "Reg.Combat.Damage.Ice"]);
    }

    #[test]
    fn declaring_after_the_child_upgrades_the_parent_and_a_repeat_is_a_duplicate() {
        let mut registry = TagRegistry::new();
        registry.declare("Up.Child", "").expect("child");
        let parent = registry.declare("Up", "Parent").expect("parent");
        assert_eq!(registry.entry(parent).expect("entry").description, "Parent");
        assert!(matches!(
            registry.declare("Up", "again"),
            Err(TagError::Duplicate { .. })
        ));
    }

    #[test]
    fn table_round_trips_through_ron() {
        let table = TagTable::from_ron(TABLE).expect("table");
        let text = table.to_ron().expect("ron");
        assert!(text.ends_with('\n'));
        assert_eq!(TagTable::from_ron(&text).expect("again"), table);
        let registry = registry();
        let rebuilt = TagRegistry::from_table(&registry.to_table()).expect("rebuilt");
        assert_eq!(rebuilt, registry);
        // Implicit parents are not written.
        assert!(registry.to_table().tags.iter().all(|d| d.name != "Reg.Status"));
    }

    #[test]
    fn a_bad_table_reports_every_problem() {
        let table = TagTable {
            version: 1,
            description: String::new(),
            tags: ["Good.Tag", "Bad Name", "Good.Tag", "A..B", "", "Fine"]
                .iter()
                .map(|n| TagDecl {
                    name: (*n).to_owned(),
                    description: String::new(),
                })
                .collect(),
        };
        let problems = table.problems();
        assert_eq!(problems.len(), 4, "{problems:?}");
        assert!(matches!(problems[0], TagError::InvalidChar { .. }));
        assert!(matches!(problems[1], TagError::Duplicate { .. }));
        assert!(matches!(problems[2], TagError::EmptySegment { .. }));
        assert!(matches!(problems[3], TagError::Empty));
        assert!(TagRegistry::from_table(&table).is_err());
    }

    #[test]
    fn newer_versions_and_garbage_are_typed_errors() {
        assert_eq!(
            TagTable::from_ron("(version: 9, tags: [])"),
            Err(TagError::Version {
                found: 9,
                expected: 1
            })
        );
        assert!(matches!(
            TagTable::from_ron("(version: 1, tags: 3)"),
            Err(TagError::Parse(_))
        ));
        assert!(matches!(TagTable::from_ron("nonsense"), Err(TagError::Parse(_))));
    }

    #[test]
    fn undeclared_usage_is_reported_per_source() {
        let registry = registry();
        let found = registry.check_names(
            "levels/arena.level.ron",
            [
                "Reg.Combat.Damage.Fire",
                "Reg.Combat.Damage.Acid",
                "Not Valid",
                "Reg.Status",
            ],
        );
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].name, "Reg.Combat.Damage.Acid");
        assert!(matches!(found[0].error, TagError::Undeclared { .. }));
        assert_eq!(found[0].source, "levels/arena.level.ron");
        assert!(matches!(found[1].error, TagError::InvalidChar { .. }));

        let container = TagContainer::from_names(["Reg.Status.Burning", "Reg.Typo"]).expect("names");
        let diagnostics = registry.check_container("hero", &container);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].name, "Reg.Typo");

        let query = super::super::TagQuery::AnyOf(vec![
            super::super::TagQuery::Has(Tag::new("Reg.Combat").expect("tag")),
            super::super::TagQuery::Has(Tag::new("Reg.Missing").expect("tag")),
        ]);
        assert_eq!(registry.check_query("rule", &query).len(), 1);
    }

    #[test]
    fn resolve_requires_a_declared_tag() {
        let registry = registry();
        assert!(registry.resolve("Reg.Status.Burning").is_ok());
        assert!(matches!(
            registry.resolve("Reg.Nope"),
            Err(TagError::Undeclared { .. })
        ));
        assert!(matches!(registry.resolve(""), Err(TagError::Empty)));
    }

    #[test]
    fn load_reads_a_file_and_reports_a_missing_one() {
        let dir = regia_test_utils::unique_temp_dir("tags_load");
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("game.tags.ron");
        let tmp = dir.join("game.tags.ron.tmp");
        std::fs::write(&tmp, TABLE).expect("write");
        std::fs::rename(&tmp, &path).expect("rename");
        assert_eq!(TagRegistry::load(&path).expect("load"), registry());
        assert!(matches!(
            TagRegistry::load(&dir.join("missing.tags.ron")),
            Err(TagError::Io { .. })
        ));
        std::fs::remove_dir_all(&dir).ok();
    }
}
