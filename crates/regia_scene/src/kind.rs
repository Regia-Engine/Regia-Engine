// SPDX-License-Identifier: MPL-2.0
//! Entity kinds: a declared label and contract per game type (`docs/specs/101-entity-kinds-and-project-map.md`,
//! ADR-2488).
//!
//! A kind names what a game object *is* (`Crate`, `EnemySpawner`) and what it must carry. It is a
//! label, not a class: kinds have no parent kinds, no inheritance between them, no fields and no
//! behavior; an entity has at most one kind and the systems that run do not depend on it.
//!
//! Two forms:
//!
//! - **Code kinds**: a zero-size marker component implementing [`EntityKind`], registered with
//!   [`KindAppExt::register_kind`]. Its contract is bevy_ecs `#[require(...)]`, read back from the
//!   component's registered required components.
//! - **Data kinds**: a `.kind.ron` file ([`file`](mod@file)) plus the reflected [`KindTag`] component naming the
//!   kind on an entity.
//!
//! A prefab root names its kind by carrying the marker or the tag; instancing copies the root's
//! components, so every instance inherits the kind. [`resolve_kind`] gives an entity's kind; an
//! instance root (which carries [`crate::prefab::PrefabInstance`] and holds the template root as a
//! child) resolves through that child.
//!
//! Entities without a kind get a derived role from [`roles`]. [`lint`] checks level files against a
//! strictness setting, [`scan`] reads a project's files and [`map`] builds the project map.

pub mod file;
pub mod lint;
pub mod map;
pub mod roles;
pub mod scan;
pub mod systems;

use regia_app::prelude::*;
use regia_ecs::ecs::component::ComponentId;
use regia_ecs::reflect::{AppTypeRegistry, GetTypeRegistration};
use serde::{Deserialize, Serialize};

use crate::prefab::{PrefabInstance, PrefabLink};

/// Longest kind name, in bytes.
pub const MAX_KIND_NAME: usize = 64;

/// Longest kind description, in characters.
pub const MAX_KIND_DESCRIPTION: usize = 120;

/// The category kinds without one are listed under.
pub const UNCATEGORIZED: &str = "Uncategorized";

/// The compile-time metadata of a code kind (see [`EntityKind`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KindMeta {
    /// Unique label, `[A-Za-z0-9_]`, 1-64 bytes.
    pub name: &'static str,
    /// Icon name from the editor's icon set (an unknown name shows the generic kind icon).
    pub icon: &'static str,
    /// sRGB color.
    pub color: [u8; 3],
    /// One line, at most 120 characters.
    pub description: &'static str,
    /// Display group, or `None` for [`UNCATEGORIZED`].
    pub category: Option<&'static str>,
}

impl KindMeta {
    /// Metadata with no category.
    #[must_use]
    pub const fn new(
        name: &'static str,
        icon: &'static str,
        color: [u8; 3],
        description: &'static str,
    ) -> Self {
        Self {
            name,
            icon,
            color,
            description,
            category: None,
        }
    }

    /// The same metadata in `category`.
    #[must_use]
    pub const fn category(mut self, category: &'static str) -> Self {
        self.category = Some(category);
        self
    }
}

/// A code kind: a zero-size marker component with its metadata.
///
/// The contract (the components the kind's entities must carry) is the marker's bevy_ecs
/// `#[require(...)]` list. Register it with [`KindAppExt::register_kind`]. The
/// [`entity_kind!`](crate::entity_kind) macro writes the impl.
pub trait EntityKind: Component {
    /// The kind's metadata.
    const KIND: KindMeta;
}

/// Implements [`EntityKind`] for a marker type in one line:
///
/// ```
/// use regia_ecs::prelude::*;
/// #[derive(Component, Default)]
/// struct Crate;
/// regia_scene::entity_kind!(Crate, "Crate", icon = "crate", color = [0xc0, 0x8a, 0x3e],
///     description = "A breakable supply crate", category = "Props");
/// assert_eq!(<Crate as regia_scene::kind::EntityKind>::KIND.category, Some("Props"));
/// ```
#[macro_export]
macro_rules! entity_kind {
    ($ty:ty, $name:literal, icon = $icon:literal, color = $color:expr, description = $desc:literal $(,)?) => {
        impl $crate::kind::EntityKind for $ty {
            const KIND: $crate::kind::KindMeta = $crate::kind::KindMeta::new($name, $icon, $color, $desc);
        }
    };
    ($ty:ty, $name:literal, icon = $icon:literal, color = $color:expr, description = $desc:literal,
     category = $cat:literal $(,)?) => {
        impl $crate::kind::EntityKind for $ty {
            const KIND: $crate::kind::KindMeta =
                $crate::kind::KindMeta::new($name, $icon, $color, $desc).category($cat);
        }
    };
}

/// The data kind an entity carries: the kind's name (declared by a `.kind.ron`).
///
/// A prefab root that carries a tag names its prefab's kind; instances inherit it because
/// instancing copies the root's components.
#[derive(Component, Reflect, Clone, Debug, Default, PartialEq, Eq)]
#[reflect(Component, Default)]
pub struct KindTag(pub String);

/// Where a kind was declared.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum KindSource {
    /// A code kind: the marker component's type path.
    Code {
        /// The marker's reflected type path (`my_game::Crate`).
        marker: String,
    },
    /// A data kind: its `.kind.ron` (project-relative, `/` separators).
    Data {
        /// The declaring file.
        path: String,
        /// The code marker the declaration is bound to, if any.
        marker: Option<String>,
    },
}

impl KindSource {
    /// The marker type path of a code kind or of a data kind bound to one.
    #[must_use]
    pub fn marker(&self) -> Option<&str> {
        match self {
            Self::Code { marker } => Some(marker),
            Self::Data { marker, .. } => marker.as_deref(),
        }
    }
}

/// One registered kind (owned, serializable).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KindInfo {
    /// Unique label.
    pub name: String,
    /// Icon name.
    pub icon: String,
    /// sRGB color.
    pub color: [u8; 3],
    /// One-line description.
    pub description: String,
    /// Display group, or `None` for [`UNCATEGORIZED`].
    pub category: Option<String>,
    /// Component type paths the kind's entities must carry, sorted.
    pub required: Vec<String>,
    /// Where it was declared.
    pub source: KindSource,
}

impl KindInfo {
    /// The category name, [`UNCATEGORIZED`] when none.
    #[must_use]
    pub fn category_name(&self) -> &str {
        self.category.as_deref().unwrap_or(UNCATEGORIZED)
    }

    /// The color as `#rrggbb`.
    #[must_use]
    pub fn color_hex(&self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.color[0], self.color[1], self.color[2])
    }

    /// Builds the info of a code kind from its metadata.
    #[must_use]
    pub fn from_meta(meta: &KindMeta, marker: String, mut required: Vec<String>) -> Self {
        required.sort();
        required.dedup();
        Self {
            name: meta.name.to_string(),
            icon: meta.icon.to_string(),
            color: meta.color,
            description: meta.description.to_string(),
            category: meta.category.map(str::to_string),
            required,
            source: KindSource::Code { marker },
        }
    }
}

/// Checks a kind name: `[A-Za-z0-9_]`, 1-[`MAX_KIND_NAME`] bytes.
///
/// # Errors
///
/// Why the name is refused.
pub fn validate_kind_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > MAX_KIND_NAME {
        return Err(format!(
            "kind name must be 1-{MAX_KIND_NAME} bytes, got {}",
            name.len()
        ));
    }
    if let Some(bad) = name.chars().find(|c| !(c.is_ascii_alphanumeric() || *c == '_')) {
        return Err(format!("kind name `{name}` has `{bad}`; use [A-Za-z0-9_]"));
    }
    Ok(())
}

/// Every kind the app knows: code kinds registered with [`KindAppExt::register_kind`] and data kinds
/// loaded from `.kind.ron` files. Sorted by name, so every listing is deterministic.
#[derive(Resource, Clone, Debug, Default)]
pub struct KindRegistry {
    kinds: Vec<KindInfo>,
    /// Marker component of each code kind: `(kind name, component id)`, sorted by name.
    markers: Vec<(String, ComponentId)>,
    /// Problems found while registering (duplicate names, drift between a code kind and its data
    /// declaration, unreadable `.kind.ron` files), in registration order.
    notes: Vec<String>,
    /// The notes that come from code registrations alone (kept by [`KindRegistry::clear_data`]).
    code_notes: Vec<String>,
}

impl KindRegistry {
    /// Every kind, sorted by name.
    #[must_use]
    pub fn kinds(&self) -> &[KindInfo] {
        &self.kinds
    }

    /// The kind called `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&KindInfo> {
        self.kinds
            .binary_search_by(|k| k.name.as_str().cmp(name))
            .ok()
            .map(|i| &self.kinds[i])
    }

    /// Registration notes (duplicates, drift).
    #[must_use]
    pub fn notes(&self) -> &[String] {
        &self.notes
    }

    /// The marker components of the code kinds, sorted by kind name.
    #[must_use]
    pub fn markers(&self) -> &[(String, ComponentId)] {
        &self.markers
    }

    /// Whether no kind is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.kinds.is_empty()
    }

    fn insert_sorted(&mut self, info: KindInfo) {
        let at = self
            .kinds
            .binary_search_by(|k| k.name.cmp(&info.name))
            .unwrap_or_else(|i| i);
        self.kinds.insert(at, info);
    }

    /// Adds a code kind with its marker component. A data declaration of the same name is
    /// replaced (the code metadata wins; a difference is noted as drift); a second code kind of
    /// the same name is refused with a note.
    pub fn register_code(&mut self, info: KindInfo, marker: ComponentId) {
        if let Some(existing) = self.get(&info.name).cloned() {
            match existing.source {
                KindSource::Code { .. } => {
                    let note = format!(
                        "kind `{}` is registered twice by code; the first registration is kept",
                        info.name
                    );
                    self.code_notes.push(note.clone());
                    self.notes.push(note);
                    return;
                }
                KindSource::Data { ref path, .. } => {
                    if let Some(drift) = drift_between(&info, &existing) {
                        self.notes.push(format!(
                            "kind `{}`: {path} differs from the code kind: {drift}",
                            info.name
                        ));
                    }
                    self.kinds.retain(|k| k.name != info.name);
                }
            }
        }
        let name = info.name.clone();
        self.insert_sorted(info);
        let at = self
            .markers
            .binary_search_by(|(n, _)| n.cmp(&name))
            .unwrap_or_else(|i| i);
        self.markers.insert(at, (name, marker));
    }

    /// Adds a data kind. A code kind of the same name keeps its place (drift is noted); a second
    /// data kind of the same name is refused with a note. Registering the same declaration again
    /// changes nothing, and a note already recorded is not repeated.
    pub fn register_data(&mut self, info: KindInfo) {
        if let Some(existing) = self.get(&info.name) {
            if *existing == info {
                return;
            }
            let note = match &existing.source {
                KindSource::Code { .. } => drift_between(existing, &info).map(|drift| {
                    let path = match &info.source {
                        KindSource::Data { path, .. } => path.as_str(),
                        KindSource::Code { .. } => "",
                    };
                    format!("kind `{}`: {path} differs from the code kind: {drift}", info.name)
                }),
                KindSource::Data { path, .. } => {
                    Some(format!("kind `{}` is declared twice; {path} is kept", info.name))
                }
            };
            if let Some(note) = note {
                self.push_note(note);
            }
            return;
        }
        self.insert_sorted(info);
    }

    /// Records a note unless the same text is already recorded.
    fn push_note(&mut self, note: String) {
        if !self.notes.contains(&note) {
            self.notes.push(note);
        }
    }

    /// Removes every data kind and every note that is not about code registrations (code kinds
    /// keep their place). Used before loading another project's `.kind.ron` files.
    pub fn clear_data(&mut self) {
        self.kinds.retain(|k| matches!(k.source, KindSource::Code { .. }));
        self.notes.clone_from(&self.code_notes);
    }

    /// Registers the data kinds of parsed `.kind.ron` files (`(project-relative path, file)`, as
    /// [`scan::scan_kind_files`] returns them) with [`KindRegistry::register_data`]. Returns how many
    /// declarations were new.
    pub fn register_data_files(&mut self, files: &[(String, file::KindFile)]) -> usize {
        let before = self.kinds.len();
        for (path, kind) in files {
            match kind.to_info(path) {
                Ok(info) => self.register_data(info),
                Err(e) => self.push_note(format!("{path}: {}: {e}", e.code())),
            }
        }
        self.kinds.len() - before
    }

    /// Notes one unreadable `.kind.ron` (`path`, error code, message).
    pub fn note_file_error(&mut self, path: &str, code: &str, message: &str) {
        self.push_note(format!("{path}: {code}: {message}"));
    }

    /// The code kind whose marker is `component`.
    #[must_use]
    pub fn kind_of_marker(&self, component: ComponentId) -> Option<&KindInfo> {
        self.markers
            .iter()
            .find(|(_, id)| *id == component)
            .and_then(|(name, _)| self.get(name))
    }

    /// The categories in display order: named categories sorted, then [`UNCATEGORIZED`] if any
    /// kind has none. Each with its kinds in name order.
    #[must_use]
    pub fn by_category(&self) -> Vec<(String, Vec<&KindInfo>)> {
        let mut groups: std::collections::BTreeMap<&str, Vec<&KindInfo>> = std::collections::BTreeMap::new();
        let mut uncategorized = Vec::new();
        for kind in &self.kinds {
            match kind.category.as_deref() {
                Some(category) => groups.entry(category).or_default().push(kind),
                None => uncategorized.push(kind),
            }
        }
        let mut out: Vec<(String, Vec<&KindInfo>)> =
            groups.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
        if !uncategorized.is_empty() {
            out.push((UNCATEGORIZED.to_string(), uncategorized));
        }
        out
    }
}

/// What differs between a code kind and its data declaration (`None` when they agree on the
/// displayed fields and the required list).
fn drift_between(code: &KindInfo, data: &KindInfo) -> Option<String> {
    let mut fields = Vec::new();
    if code.icon != data.icon {
        fields.push("icon");
    }
    if code.color != data.color {
        fields.push("color");
    }
    if code.description != data.description {
        fields.push("description");
    }
    if code.category != data.category {
        fields.push("category");
    }
    if code.required != data.required {
        fields.push("required");
    }
    (!fields.is_empty()).then(|| fields.join(", "))
}

/// Registering code kinds on an [`App`].
pub trait KindAppExt {
    /// Registers the code kind `T`: its component, its reflection and a [`KindRegistry`] entry
    /// whose required list is the marker's direct `#[require(...)]` components.
    ///
    /// The marker must be zero-size (checked at compile time), so a kind adds no per-entity memory.
    fn register_kind<T: EntityKind + GetTypeRegistration>(&mut self) -> &mut Self;
}

impl KindAppExt for App {
    fn register_kind<T: EntityKind + GetTypeRegistration>(&mut self) -> &mut Self {
        const {
            assert!(
                std::mem::size_of::<T>() == 0,
                "an entity kind marker must be a zero-size type"
            );
        }
        self.register_type::<T>();
        let world = self.world_mut();
        let id = world.register_component::<T>();
        let registry = world.get_resource::<AppTypeRegistry>().cloned();
        let marker = type_path_of_component(world, registry.as_ref(), id)
            .unwrap_or_else(|| std::any::type_name::<T>().to_string());
        let required: Vec<String> = world
            .components()
            .get_info(id)
            .map(|info| {
                info.required_components()
                    .iter_direct_ids()
                    .filter_map(|req| type_path_of_component(world, registry.as_ref(), req))
                    .collect()
            })
            .unwrap_or_default();
        let info = KindInfo::from_meta(&T::KIND, marker, required);
        if let Err(problem) = validate_kind_name(&info.name) {
            tracing::warn!(target: "regia_scene::kind", "{problem}");
        }
        world.init_resource::<KindRegistry>();
        world.resource_mut::<KindRegistry>().register_code(info, id);
        self
    }
}

/// A component's reflected type path (the name scene files use), else its debug name.
#[must_use]
pub fn type_path_of_component(
    world: &World,
    registry: Option<&AppTypeRegistry>,
    id: ComponentId,
) -> Option<String> {
    let info = world.components().get_info(id)?;
    if let (Some(type_id), Some(registry)) = (info.type_id(), registry)
        && let Some(registration) = registry.read().get(type_id)
    {
        return Some(registration.type_info().type_path().to_string());
    }
    // Without bevy's `debug` feature the name is a placeholder in angle brackets: no name then.
    let name = info.name().to_string();
    (!name.starts_with('<')).then_some(name)
}

/// An entity's resolved kind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedKind {
    /// The kind's name.
    pub name: String,
    /// The registered kind, or `None` when a [`KindTag`] names no known kind (`kind_unknown`).
    pub info: Option<KindInfo>,
    /// Further code markers on the same entity (`kind_conflict`), in name order.
    pub conflicts: Vec<String>,
    /// Whether the kind comes from the instance's template root (an instance root without a kind
    /// of its own).
    pub via_prefab: bool,
}

/// The kind an entity carries itself: its code markers (by kind name; the first wins, the rest are
/// conflicts), else its [`KindTag`].
fn own_kind(registry: &KindRegistry, world: &World, entity: Entity) -> Option<ResolvedKind> {
    let entity_ref = world.get_entity(entity).ok()?;
    let mut markers = registry
        .markers
        .iter()
        .filter(|(_, id)| entity_ref.contains_id(*id))
        .map(|(name, _)| name.clone());
    if let Some(first) = markers.next() {
        return Some(ResolvedKind {
            info: registry.get(&first).cloned(),
            name: first,
            conflicts: markers.collect(),
            via_prefab: false,
        });
    }
    let tag = entity_ref.get::<KindTag>()?;
    Some(ResolvedKind {
        name: tag.0.clone(),
        info: registry.get(&tag.0).cloned(),
        conflicts: Vec::new(),
        via_prefab: false,
    })
}

/// The entity's kind: its own (see the module docs), else, on a prefab instance root, the kind of
/// the template root hanging under it. `None` when it has no kind.
#[must_use]
pub fn resolve_kind(registry: &KindRegistry, world: &World, entity: Entity) -> Option<ResolvedKind> {
    if let Some(own) = own_kind(registry, world, entity) {
        return Some(own);
    }
    world.get::<PrefabInstance>(entity)?;
    let children = world.get::<Children>(entity)?;
    let mut template_roots: Vec<Entity> = children
        .iter()
        .filter(|child| world.get::<PrefabLink>(*child).is_some())
        .collect();
    template_roots.sort_by_key(|e| e.to_bits());
    template_roots.into_iter().find_map(|child| {
        own_kind(registry, world, child).map(|mut kind| {
            kind.via_prefab = true;
            kind
        })
    })
}

/// [`resolve_kind`] with the world's [`KindRegistry`] (an empty one when none is installed).
#[must_use]
pub fn kind_of(world: &World, entity: Entity) -> Option<ResolvedKind> {
    match world.get_resource::<KindRegistry>() {
        Some(registry) => resolve_kind(registry, world, entity),
        None => resolve_kind(&KindRegistry::default(), world, entity),
    }
}

/// What [`load_project_kinds`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KindLoad {
    /// Data kinds now registered from the project's files.
    pub loaded: usize,
    /// Files that could not be read: project-relative path, error code (`kind_parse`,
    /// `kind_version_newer`, `kind_bad_param`) and message.
    pub errors: Vec<scan::KindFileIssue>,
}

/// Loads the `.kind.ron` declarations under `<root>/assets` into the world's [`KindRegistry`]
/// (installing one if needed), replacing the data kinds of a previously loaded project. Code kinds
/// keep their place; a code kind and its declaration that differ get a drift note. Unreadable files
/// become registry notes and are returned. Reads only `.kind.ron` files, and nothing without an
/// `assets` folder (spec 101 § 2.3, ADR-5609).
pub fn load_project_kinds(world: &mut World, root: &std::path::Path) -> KindLoad {
    let (kinds, errors) = scan::scan_kind_files(root);
    world.init_resource::<KindRegistry>();
    let mut registry = world.resource_mut::<KindRegistry>();
    registry.clear_data();
    registry.register_data_files(&kinds);
    for (path, code, message) in &errors {
        registry.note_file_error(path, code, message);
    }
    let loaded = registry
        .kinds()
        .iter()
        .filter(|k| matches!(k.source, KindSource::Data { .. }))
        .count();
    KindLoad { loaded, errors }
}

/// Registers the kind types and resources (called by [`crate::ScenePlugin`]).
pub(crate) fn add_to_app(app: &mut App) {
    app.register_type::<KindTag>().init_resource::<KindRegistry>();
}

#[cfg(test)]
mod tests;
