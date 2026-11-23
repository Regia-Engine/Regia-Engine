// SPDX-License-Identifier: MPL-2.0
//! Identity and classification components (`docs/specs/14-scene-and-framework.md` A1).

use regia_core::id::{Guid, Name as NameString};
use regia_core::math::{Quat, Vec2, Vec3, Vec4};
use regia_ecs::prelude::*;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

/// A human-readable entity name, shown in the editor outliner and by `find_by_name`.
///
/// Wraps the interned [`regia_core::id::Name`] string; cloning shares the allocation.
/// Scene files store it in the entity's `name` field, not in `components`.
///
/// Reflection treats it as opaque and its serde impl is a **transparent string**, so canonical
/// RON writes `"Door"` (not `("Door")`) and a list of names is `["crate", "loot"]`.
#[derive(Component, Reflect, Clone, Default, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[reflect(Component, Default, Clone, Serialize, Deserialize, opaque)]
pub struct Name(pub NameString);

impl Name {
    /// Creates a name from anything string-like.
    pub fn new(name: impl Into<NameString>) -> Self {
        Self(name.into())
    }

    /// The name as a string slice.
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl std::fmt::Display for Name {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Transparent-string serde: canonical RON writes `Name` as `"Door"` (see the type docs).
impl Serialize for Name {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Name {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self(NameString::deserialize(deserializer)?))
    }
}

/// A stable identity for entities that survives save/load and level moves.
///
/// Scene files store it in the entity's `guid` field; prefab-internal entities use it as the
/// stable *local id* that overrides address (`docs/specs/14-scene-and-framework.md` A2).
#[derive(
    Component,
    Reflect,
    Clone,
    Copy,
    Default,
    Debug,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
)]
#[reflect(Component, Default, Clone, Serialize, Deserialize, opaque)]
pub struct EntityGuid(pub Guid);

impl EntityGuid {
    /// A random GUID, for entities created outside a scene load.
    pub fn new_random() -> Self {
        Self(Guid::new_random())
    }
}

impl std::fmt::Display for EntityGuid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Free-form tags for grouping entities in queries and editor filters.
///
/// Serialises as a list of strings: `(["crate", "loot"])`. The column keeps a summary change
/// tick so a feature pack can skip its tag walk on a frame where no tag was added, edited or
/// removed.
#[derive(Component, Reflect, Clone, Default, Debug, PartialEq, Eq, Hash)]
#[reflect(Component, Default)]
#[component(summary_tick)]
pub struct Tags(pub SmallVec<[Name; 4]>);

impl Tags {
    /// Tags from an iterator of string-like values.
    pub fn from_iter_str<'a>(tags: impl IntoIterator<Item = &'a str>) -> Self {
        Self(tags.into_iter().map(Name::new).collect())
    }

    /// True if `tag` is present.
    pub fn contains(&self, tag: &str) -> bool {
        self.0.iter().any(|t| t.as_str() == tag)
    }
}

/// Per-domain membership bit masks (render, physics, gameplay, ...) sharing layer **names** but
/// keeping separate bit sets. Bits are assigned by the owning subsystem; the scene only stores
/// them. Entities without `Layers` are members of layer 0 in every domain.
#[derive(Component, Reflect, Clone, Copy, Default, Debug, PartialEq, Eq, Hash)]
#[reflect(Component, Default)]
pub struct Layers(pub u32);

/// Which map layer an entity belongs to: the floor of a stacked map (terrain surface, a tunnel
/// level, an upper deck). Terrain is layer 0; a bake assigns the layer per entity. Entities (and
/// their descendants) without a `MapLayer` take the nearest ancestor's layer, else layer 0 (a
/// `Terrain` is always layer 0). `regia_pick::world_pick` reports the
/// layer it hit and can restrict a ray to a set of layers.
#[derive(Component, Reflect, Clone, Copy, Default, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[reflect(Component, Default)]
pub struct MapLayer(pub u8);

/// Which indoor visibility cell an entity belongs to (`docs/specs/25-sim-bridge-and-do27.md`
/// § 7.7, ADR-7586): an id from the map's `<map>.viscells.ron`. Cell 0 is outdoors. The renderer
/// walks the portals from each view and leaves out of the colour pass the draws of an entity whose
/// cell no view reaches (`regia_render::viscells`). An entity without `VisCell` takes its nearest
/// tagged ancestor's (stage 2, as `MapLayer` does); an entity with none anywhere up its chain
/// always draws.
#[derive(Component, Reflect, Clone, Copy, Default, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[reflect(Component, Default)]
pub struct VisCell(pub u16);

/// Marks an entity a procedural generator made (`docs/specs/62-procedural-content-graph.md` § 7.1).
///
/// Generated entities are output, not authored content: [`crate::spawn::snapshot_scene`] (and so
/// the level saver) never writes them, and the generator that owns them removes them when it
/// clears or re-runs. The marker lives here, below the generator crate, so saving needs no
/// dependency on it. Runtime-only: not reflected and not part of the scene format.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Generated {
    /// The `EntityGuid` of the generator entity that produced this entity.
    pub generator: Guid,
    /// The run that produced it (the generator's run counter, starting at 1).
    pub run_id: u64,
}

/// Marks an entity the DCC scene feed owns (`docs/specs/60-dcc-scene-feed.md` § 2.4): it is read-only
/// to scene edits (`feed_owned`) and [`crate::spawn::snapshot_scene`] never writes it. The marker
/// lives here, below the feed, so saving and the edit guards need no dependency on it.
/// Runtime-only: not reflected and not part of the scene format.
#[derive(Component, Clone, Debug, PartialEq, Eq, Hash)]
pub struct FeedOwned {
    /// The sender's stable item key.
    pub key: String,
}

/// Saved on a user entity parented under a feed-owned entity, in place of its parent link (spec 60
/// § 2.4 rule 3). When an item with this `key` exists the entity is re-parented to it, keeping its
/// local transform; otherwise it stays at the scene root.
#[derive(Component, Reflect, Clone, Debug, Default, PartialEq, Eq, Hash)]
#[reflect(Component, Default)]
pub struct FeedAttach {
    /// The item key this entity hangs under.
    pub key: String,
}

/// Marks an entity as immobile: its [`crate::Transform`] is not expected to change at runtime,
/// which enables static batching and shadow-map caching. The editor warns when a `Static` entity
/// is moved at runtime.
#[derive(Component, Reflect, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[reflect(Component, Default)]
pub struct Static;

/// Whether an entity's lighting is baked or live: the baked-GI export reads the `Static`
/// entities (their meshes become bake geometry and emitters), and `Dynamic` ones are lit at
/// runtime from the baked volume without being part of it (`book/agents/baked-gi-maps.md`).
///
/// The default is `Dynamic`, so an entity without the component, and every existing scene, is
/// unchanged. It is separate from [`Static`], which promises only that the transform does not
/// change (batching, shadow caching): a `GiMobility::Static` entity should not move at runtime
/// either, because the bake would no longer match the frame.
#[derive(Component, Reflect, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[reflect(Component, Default)]
pub enum GiMobility {
    /// Part of the bake: geometry and emitters the baked volume was computed from.
    Static,
    /// Lit at runtime, not part of the bake. The default.
    #[default]
    Dynamic,
}

/// How a `GiMobility::Static` mesh entity takes part in `regia bake-lightmaps --scene` (ADR-2024).
/// Without the component it receives at the bake's density: `density_scale` 1.
#[derive(Component, Reflect, Clone, Copy, Debug, PartialEq)]
#[reflect(Component, Default)]
pub struct LightmapSurface {
    /// Multiplier on the bake's texels per metre for this entity: above 1 for hero pieces, below 1
    /// for background. Must be finite and above 0; the bake refuses anything else.
    pub density_scale: f32,
    /// `false` leaves the entity to the probe light; it still shadows and bounces in the bake.
    pub receive: bool,
}

impl Default for LightmapSurface {
    fn default() -> Self {
        Self {
            density_scale: 1.0,
            receive: true,
        }
    }
}

/// Authoritative visibility request, inherited down the hierarchy by
/// [`crate::propagate::propagate_transforms`] into [`InheritedVisibility`].
///
/// - [`Visibility::Inherited`] (default): use the parent's visibility.
/// - [`Visibility::Visible`]: visible regardless of the parent.
/// - [`Visibility::Hidden`]: hidden regardless of the parent.
#[derive(Component, Reflect, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[reflect(Component, Default)]
#[require(InheritedVisibility, ViewVisibility)]
pub enum Visibility {
    /// Use the parent's resolved visibility (default).
    #[default]
    Inherited,
    /// Visible even if the parent is hidden.
    Visible,
    /// Hidden even if the parent is visible.
    Hidden,
}

/// The resolved visibility of an entity after inheritance, computed by
/// [`crate::propagate::propagate_transforms`]. Entities with a [`Visibility`] get one via
/// required components (default `true` until the first propagation run).
#[derive(Component, Reflect, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[reflect(Component, Default)]
pub struct InheritedVisibility(pub bool);

impl Default for InheritedVisibility {
    fn default() -> Self {
        Self(true)
    }
}

/// Whether the entity is rendered by at least one view this frame. Set by the renderer's culling
/// systems (`specs/13`), not by the scene: it stays `false` until a view claims the entity.
#[derive(Component, Reflect, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[reflect(Component, Default)]
pub struct ViewVisibility(pub bool);

/// Registers the primitive and math types that scene data (component fields, prefab override
/// values) can reference. A type must be registered to be (de)serialized reflectively; the
/// plugin calls this, and games extend the registry with their own types.
pub fn register_primitives(registry: &mut TypeRegistry) {
    registry.register::<f32>();
    registry.register::<f64>();
    registry.register::<bool>();
    registry.register::<char>();
    registry.register::<String>();
    registry.register::<i8>();
    registry.register::<i16>();
    registry.register::<i32>();
    registry.register::<i64>();
    registry.register::<i128>();
    registry.register::<u8>();
    registry.register::<u16>();
    registry.register::<u32>();
    registry.register::<u64>();
    registry.register::<u128>();
    registry.register::<usize>();
    registry.register::<Vec2>();
    registry.register::<Vec3>();
    registry.register::<Vec4>();
    registry.register::<Quat>();
}

/// Morph (blend shape) weights of a mesh entity: one per morph target, target-index order
/// (`specs/17` § 4). Animation writes them (`regia_animation` re-exports this type); the
/// renderer deforms the entity's mesh with them when the entity also has morph targets
/// (`regia_render::MeshMorphTargets`). It lives in the scene crate so the renderer reads it
/// without depending on animation. A missing weight reads as zero.
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct MorphWeights(pub Vec<f32>);

/// Largest morph-target count [`MorphWeights::set`] will grow to.
pub const MAX_MORPH_TARGETS: usize = 4096;

impl MorphWeights {
    /// The weight of target `index` (zero past the end).
    #[must_use]
    pub fn get(&self, index: usize) -> f32 {
        self.0.get(index).copied().unwrap_or(0.0)
    }

    /// Sets target `index` to `weight`, growing the list with zeros as needed. Returns false
    /// (nothing changes) for an index at or past [`MAX_MORPH_TARGETS`], so an untrusted index
    /// cannot request a huge allocation.
    pub fn set(&mut self, index: usize, weight: f32) -> bool {
        if index >= MAX_MORPH_TARGETS {
            return false;
        }
        if index >= self.0.len() {
            self.0.resize(index + 1, 0.0);
        }
        self.0[index] = weight;
        true
    }

    /// The weight of the target named `name` in `names`; `None` for an unknown name.
    #[must_use]
    pub fn get_named(&self, names: &MorphTargetNames, name: &str) -> Option<f32> {
        names.index_of(name).map(|index| self.get(index))
    }

    /// Sets the target named `name` in `names`; false (nothing changes) for an unknown name.
    pub fn set_named(&mut self, names: &MorphTargetNames, name: &str, weight: f32) -> bool {
        match names.index_of(name) {
            Some(index) => self.set(index, weight),
            None => false,
        }
    }
}

/// Names of a mesh entity's morph targets, target-index order (glTF `extras.targetNames`, or
/// `target_<i>`): what name-based weight lookups resolve against ([`MorphWeights::set_named`]).
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct MorphTargetNames(pub std::sync::Arc<[String]>);

impl MorphTargetNames {
    /// Names in target-index order.
    pub fn new<S: Into<String>>(names: impl IntoIterator<Item = S>) -> Self {
        Self(names.into_iter().map(Into::into).collect())
    }

    /// The index of the target named `name`.
    #[must_use]
    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.0.iter().position(|n| n == name)
    }

    /// Number of names.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether there are no names.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// The editor Outliner's folder of a top-level entity (ADR-7757): `path` is a `/`-separated
/// folder path such as `"Lights/Street"`. Folders are drawn as rows in the editor's tree and hold
/// entities without being entities themselves. Saved with the level so the tree keeps its folders;
/// no runtime system reads it (a game can ignore it), so it changes nothing at play time.
#[derive(Component, Reflect, Clone, Default, Debug, PartialEq, Eq, Hash)]
#[reflect(Component, Default)]
pub struct EditorFolder {
    /// The folder path, outermost folder first, `/`-separated (no leading or trailing `/`).
    pub path: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn morph_weights_set_rejects_huge_indices() {
        let mut weights = MorphWeights::default();
        assert!(!weights.set(usize::MAX / 2, 1.0));
        assert!(!weights.set(MAX_MORPH_TARGETS, 1.0));
        assert!(weights.0.is_empty());
        assert!(weights.set(MAX_MORPH_TARGETS - 1, 0.5));
        assert_eq!(weights.0.len(), MAX_MORPH_TARGETS);
        assert_eq!(weights.get(MAX_MORPH_TARGETS - 1), 0.5);
    }

    #[test]
    fn name_round_trips_through_reflected_ron() {
        let mut registry = TypeRegistry::default();
        register_primitives(&mut registry);
        registry.register::<Name>();

        let value = Name::new("Player Start");
        let text = regia_ecs::serialize::ReflectCodec::new(&registry)
            .to_ron_typed(&value)
            .expect("serialize");
        assert_eq!(text.trim(), r#""Player Start""#);
        let registration = registry
            .get_with_type_path("regia_scene::components::Name")
            .expect("reg");
        let mut codec = regia_ecs::serialize::ReflectCodec::new(&registry);
        let back = codec.from_ron_typed(&text, registration).expect("deserialize");
        let back = back.downcast::<Name>().expect("concrete");
        assert_eq!(*back, value);
    }

    #[test]
    fn entity_guid_serializes_as_hex_string() {
        let mut registry = TypeRegistry::default();
        register_primitives(&mut registry);
        registry.register::<EntityGuid>();

        let guid = EntityGuid(Guid::new_random());
        let text = regia_ecs::serialize::ReflectCodec::new(&registry)
            .to_ron_typed(&guid)
            .expect("serialize");
        assert_eq!(text.trim(), format!(r#"("{}")"#, guid.0));
        let registration = registry
            .get_with_type_path("regia_scene::components::EntityGuid")
            .expect("reg");
        let mut codec = regia_ecs::serialize::ReflectCodec::new(&registry);
        let back = codec
            .from_ron_typed(&text, registration)
            .expect("deserialize")
            .downcast::<EntityGuid>()
            .expect("concrete");
        assert_eq!(*back, guid);
    }

    #[test]
    fn tags_parse_from_string_list() {
        // `Tags` holds a list of `Name`s, which are transparent strings: the canonical form of
        // the whole component is `(["crate", "loot"])` (what prefab overrides author).
        let mut registry = TypeRegistry::default();
        register_primitives(&mut registry);
        registry.register::<Tags>();
        registry.register::<Name>();
        registry.register::<SmallVec<[Name; 4]>>();
        let mut codec = regia_ecs::serialize::ReflectCodec::new(&registry);
        let registration = registry
            .get_with_type_path("regia_scene::components::Tags")
            .expect("reg");
        let value = codec
            .from_ron_typed(r#"(["crate", "loot"])"#, registration)
            .expect("parse")
            .downcast::<Tags>()
            .expect("concrete");
        assert_eq!(
            *value,
            Tags(SmallVec::from_vec(vec![Name::new("crate"), Name::new("loot")]))
        );
        let text = codec
            .to_ron_typed(&Tags::from_iter_str(["crate", "loot"]))
            .expect("write");
        // Canonical RON: one item per line (the compact single-line form parses identically).
        assert_eq!(text.trim(), "([\n    \"crate\",\n    \"loot\",\n])");
    }

    #[test]
    fn tags_carry_names() {
        let tags = Tags::from_iter_str(["crate", "loot"]);
        assert!(tags.contains("crate"));
        assert!(!tags.contains("prop"));
        assert_eq!(tags.0.len(), 2);
    }

    #[test]
    fn visibility_defaults_to_inherited() {
        assert_eq!(Visibility::default(), Visibility::Inherited);
        assert!(InheritedVisibility::default().0);
        assert!(!ViewVisibility::default().0);
    }

    /// An entity without the component is `Dynamic`, and the component round-trips through a
    /// scene file by its type path.
    #[test]
    fn gi_mobility_defaults_to_dynamic_and_round_trips_through_a_scene() {
        use crate::format::SceneFile;
        use crate::spawn::{SnapshotFilter, SpawnOptions, snapshot_scene, spawn_scene};
        use regia_ecs::reflect::AppTypeRegistry;
        assert_eq!(GiMobility::default(), GiMobility::Dynamic);

        let guid = Guid::new_random();
        let text = format!(
            r#"(
    format_version: 10,
    entities: [
        (guid: "{guid}", components: {{
            "regia_scene::components::GiMobility": Static,
        }}),
    ],
)"#
        );
        let file = SceneFile::from_ron_str(&text).expect("parse");
        let mut world = World::new();
        world.insert_resource(AppTypeRegistry::default());
        {
            let handle = world.resource::<AppTypeRegistry>().0.clone();
            let mut registry = handle.write();
            register_primitives(&mut registry);
            registry.register::<EntityGuid>();
            registry.register::<GiMobility>();
        }
        spawn_scene(&mut world, &file, &SpawnOptions::default()).expect("spawn");
        let mobility: Vec<GiMobility> = world.query::<&GiMobility>().iter(&world).copied().collect();
        assert_eq!(mobility, vec![GiMobility::Static]);
        let snapshot = snapshot_scene(&mut world, &SnapshotFilter::default()).expect("snapshot");
        let out = snapshot.to_ron_string().expect("write");
        assert!(out.contains("GiMobility"), "{out}");
    }
}
