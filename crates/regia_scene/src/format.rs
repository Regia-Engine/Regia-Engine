// SPDX-License-Identifier: MPL-2.0
//! The `.level.ron` / `.prefab.ron` file format. The current scene format is version 14
//! ([`SCENE_FORMAT_VERSION`]; `docs/specs/14-scene-and-framework.md` A2; published as
//! `docs/formats/scene-files.md` + JSON Schema).
//!
//! A scene file is a list of entity records, each carrying a stable `guid`, an optional parent
//! `guid`, a map of **type-path-keyed** component values (raw RON, decoded through reflection on
//! load), and optionally a `prefab_instance` record. Prefab-internal entities are addressed by
//! their guid (the *local id*), never by name. The same structure serves both file kinds: a
//! `.prefab.ron` is a scene file with exactly one root entity.
//!
//! Every file carries `format_version`. Loaders reject newer versions with a clear error and
//! migrate older ones through [`migrate_to_latest`]; a change ships with a migrator
//! (`docs/specs/12-assets.md` § 4a).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::str::FromStr;

use regia_core::id::Guid;
use serde::ser::SerializeStruct;
use serde::{Deserialize, Serialize};

/// The scene file format version this build reads and writes.
pub const SCENE_FORMAT_VERSION: u32 = 14;

/// Errors from reading, writing or validating a [`SceneFile`].
#[derive(Debug, thiserror::Error)]
pub enum SceneFormatError {
    /// RON serialization failed.
    #[error("RON: {0}")]
    Ron(#[from] ron::Error),
    /// RON parsing failed (with source position).
    #[error("RON: {0}")]
    RonParse(#[from] ron::error::SpannedError),
    /// postcard (de)serialization failed.
    #[error("postcard: {0}")]
    Postcard(#[from] postcard::Error),
    /// A `asset:` reference could not be parsed.
    #[error("bad asset reference `{0}` (expected `asset:<guid>` or `<guid>`)")]
    BadAssetRef(String),
    /// The file's format version is newer than this build understands.
    #[error(
        "scene file format version {found} is newer than supported version {supported}; upgrade the engine"
    )]
    UnsupportedVersion {
        /// Version found in the file.
        found: u32,
        /// Newest version this build supports.
        supported: u32,
    },
    /// A migration step failed.
    #[error("migration v{from} -> v{to} failed: {reason}")]
    Migration {
        /// Source version.
        from: u32,
        /// Target version.
        to: u32,
        /// Why it failed.
        reason: String,
    },
    /// The file's structure is invalid (duplicate guids, unknown parents, cycles, ...).
    #[error("invalid scene: {0}")]
    Invalid(String),
}

/// A serializable reference to an asset by id: `"asset:44fe…"` (the `asset:` prefix is optional
/// when reading). Until the asset server lands (`specs/12` § 3) the scene stores bare asset ids;
/// `AssetRef<T>` will wrap the same value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct SceneAssetRef(pub Guid);

impl SceneAssetRef {
    /// A new asset reference.
    pub const fn new(guid: Guid) -> Self {
        Self(guid)
    }

    /// The referenced asset's id.
    pub const fn guid(&self) -> Guid {
        self.0
    }
}

impl std::fmt::Display for SceneAssetRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "asset:{}", self.0)
    }
}

impl FromStr for SceneAssetRef {
    type Err = SceneFormatError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let guid = s.strip_prefix("asset:").unwrap_or(s);
        guid.parse()
            .map(Self)
            .map_err(|_| SceneFormatError::BadAssetRef(s.to_owned()))
    }
}

impl Serialize for SceneAssetRef {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for SceneAssetRef {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// One entity record: identity, optional parent, component values (type path → raw RON) and an
/// optional prefab instance.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct SceneEntity {
    /// Stable identity; within a prefab this doubles as the *local id* overrides address.
    pub guid: Guid,
    /// Human-readable name (editor outliner, `find_by_name`). Not an addressing key.
    #[serde(default, deserialize_with = "crate::format::lenient_option::deserialize")]
    pub name: Option<String>,
    /// Parent entity guid, or `None` for a root. May reference any entity in the file.
    #[serde(default, deserialize_with = "crate::format::lenient_option::deserialize")]
    pub parent: Option<Guid>,
    /// Component values by full type path, as raw RON. Decoded through reflection on load;
    /// unknown types are skipped with a warning (or preserved, editor policy).
    #[serde(default)]
    pub components: BTreeMap<String, Box<ron::value::RawValue>>,
    /// Prefab instantiation record, if this entity instantiates a prefab.
    #[serde(default, deserialize_with = "crate::format::lenient_option::deserialize")]
    pub prefab_instance: Option<PrefabInstanceData>,
}

impl Serialize for SceneEntity {
    /// Text (RON) omits `None` optionals so files stay diff-friendly; binary (postcard) must
    /// emit every field — it is positional and cannot skip.
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let human_readable = serializer.is_human_readable();
        let mut state = serializer.serialize_struct("SceneEntity", 5)?;
        state.serialize_field("guid", &self.guid)?;
        match &self.name {
            Some(name) if human_readable => state.serialize_field("name", name)?,
            _ if human_readable => state.skip_field("name")?,
            _ => state.serialize_field("name", &self.name)?,
        }
        match &self.parent {
            Some(parent) if human_readable => state.serialize_field("parent", parent)?,
            _ if human_readable => state.skip_field("parent")?,
            _ => state.serialize_field("parent", &self.parent)?,
        }
        state.serialize_field("components", &self.components)?;
        match &self.prefab_instance {
            Some(instance) if human_readable => state.serialize_field("prefab_instance", instance)?,
            _ if human_readable => state.skip_field("prefab_instance")?,
            _ => state.serialize_field("prefab_instance", &self.prefab_instance)?,
        }
        state.end()
    }
}

/// A prefab instantiation: which prefab, which overrides, and structural additions/removals.
/// The entity carrying this record is the *instance root*; the instantiated subtree is parented
/// under it. See `docs/formats/scene-files.md` for override addressing rules.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PrefabInstanceData {
    /// The prefab asset to instantiate.
    pub prefab: SceneAssetRef,
    /// Per-field value overrides on prefab-internal entities.
    #[serde(default)]
    pub overrides: Vec<PrefabOverride>,
    /// Components added to prefab-internal entities that the prefab itself does not define.
    #[serde(default)]
    pub added_components: Vec<PrefabComponentAdd>,
    /// Components removed from prefab-internal entities.
    #[serde(default)]
    pub removed_components: Vec<PrefabComponentRemove>,
    /// Whole entity subtrees attached under prefab-internal entities (or the instance root).
    #[serde(default, deserialize_with = "crate::format::bounded_children::deserialize")]
    pub added_children: Vec<SceneEntity>,
    /// Prefab-internal local ids (with their subtrees) not to instantiate.
    #[serde(default)]
    pub removed_children: Vec<Guid>,
}

impl PrefabInstanceData {
    /// An instance of `prefab` with no overrides.
    pub fn plain(prefab: SceneAssetRef) -> Self {
        Self {
            prefab,
            overrides: Vec::new(),
            added_components: Vec::new(),
            removed_components: Vec::new(),
            added_children: Vec::new(),
            removed_children: Vec::new(),
        }
    }
}

/// One field override on one prefab-internal entity.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PrefabOverride {
    /// The target entity's local id (guid) inside the prefab.
    pub target: Guid,
    /// The component's full type path.
    pub component: String,
    /// The field path within the component, e.g. `.intensity`. Empty (or `.`) replaces the whole
    /// component value.
    pub path: String,
    /// The override value as raw RON, decoded against the field's type on load.
    pub value: Box<ron::value::RawValue>,
}

/// A component added to a prefab-internal entity by an instance.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PrefabComponentAdd {
    /// The target entity's local id (guid) inside the prefab.
    pub target: Guid,
    /// The component's full type path.
    pub component: String,
    /// The value as raw RON.
    pub value: Box<ron::value::RawValue>,
}

/// A component removed from a prefab-internal entity by an instance.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PrefabComponentRemove {
    /// The target entity's local id (guid) inside the prefab.
    pub target: Guid,
    /// The component's full type path.
    pub component: String,
}

/// The top-level document of `.level.ron` and `.prefab.ron` files.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SceneFile {
    /// Format version; loaders reject newer versions and migrate older ones.
    pub format_version: u32,
    /// The entity records. Order is irrelevant except that parents must exist somewhere.
    #[serde(default)]
    pub entities: Vec<SceneEntity>,
    /// Resource values by full type path (e.g. `LevelSettings`), as raw RON.
    #[serde(default)]
    pub resources: BTreeMap<String, Box<ron::value::RawValue>>,
}

impl SceneFile {
    /// An empty file at the current format version.
    pub fn new() -> Self {
        Self {
            format_version: SCENE_FORMAT_VERSION,
            ..Self::default()
        }
    }

    /// Serializes to the canonical RON text of a `.level.ron` / `.prefab.ron` file.
    ///
    /// The text starts with `SceneFile(`; parsing also accepts the bare `(` form.
    pub fn to_ron_string(&self) -> Result<String, SceneFormatError> {
        let mut text = ron::ser::to_string_pretty(self, regia_ecs::serialize::canonical_pretty_config())?;
        text.push('\n');
        // RON omits struct names by default; the published format shows the document name.
        if let Some(rest) = text.strip_prefix('(') {
            text = format!("SceneFile({rest}");
        }
        Ok(text)
    }

    /// Parses a `.level.ron` / `.prefab.ron` file, rejecting or migrating by format version.
    ///
    /// The `format_version` field is peeked before the full deserialise, so a *newer* version is
    /// rejected even when its body no longer matches the current struct.
    ///
    /// Loading time is linear in the file size: the `entities` list is split lexically and each
    /// record is parsed on its own (see `text_split` for why one whole-document `ron` parse is
    /// quadratic). Error positions refer to the whole text either way.
    pub fn from_ron_str(text: &str) -> Result<Self, SceneFormatError> {
        let _span = tracing::info_span!("scene_from_ron_str", bytes = text.len()).entered();
        let split = {
            let _span = tracing::info_span!("scene_text_split").entered();
            crate::text_split::split_scene_text(text)
        };
        let mut file = match split {
            Some(split) => {
                check_text_version(&split.header)?;
                let mut file: SceneFile = ron::from_str(&split.header)?;
                let _span =
                    tracing::info_span!("scene_parse_entities", count = split.entities.len()).entered();
                file.entities = split
                    .entities
                    .iter()
                    .map(|range| {
                        ron::from_str::<SceneEntity>(&text[range.clone()])
                            .map_err(|e| crate::text_split::relocate_error(e, text, range.start).into())
                    })
                    .collect::<Result<_, SceneFormatError>>()?;
                file
            }
            // Not a plain document (extensions, malformed brackets, ...): one whole parse, which
            // also reports the precise error.
            None => {
                check_text_version(text)?;
                ron::from_str(text)?
            }
        };
        file.finish_load()?;
        Ok(file)
    }

    /// Serializes to postcard bytes (the authoring structure; see [`crate::cooked`] for the cooked
    /// runtime form with reflected payloads and a string table).
    pub fn to_postcard(&self) -> Result<Vec<u8>, SceneFormatError> {
        Ok(postcard::to_allocvec(self)?)
    }

    /// Parses postcard bytes written by [`SceneFile::to_postcard`], with the same version rules
    /// as [`SceneFile::from_ron_str`].
    pub fn from_postcard(bytes: &[u8]) -> Result<Self, SceneFormatError> {
        // `format_version` is the first field, and a newer body may not decode: read it first so a
        // newer file reads as what it is, like the text path's peek.
        if let Ok((found, _)) = postcard::take_from_bytes::<u32>(bytes)
            && found > SCENE_FORMAT_VERSION
        {
            return Err(SceneFormatError::UnsupportedVersion {
                found,
                supported: SCENE_FORMAT_VERSION,
            });
        }
        let mut file: SceneFile = postcard::from_bytes(bytes)?;
        file.finish_load()?;
        Ok(file)
    }

    /// Post-parse steps shared by both readers: trim the captured raw values (their surrounding
    /// whitespace depends on where the value sat in the file), reject newer versions, migrate
    /// and validate.
    pub(crate) fn finish_load(&mut self) -> Result<(), SceneFormatError> {
        self.normalize_and_migrate()?;
        self.validate()?;
        Ok(())
    }

    /// `finish_load` without the final validation: the level-directory upgrade migrates groups of
    /// entity records that are only valid once reassembled.
    pub(crate) fn normalize_and_migrate(&mut self) -> Result<(), SceneFormatError> {
        fn trim_map(map: &mut BTreeMap<String, Box<ron::value::RawValue>>) {
            for value in map.values_mut() {
                *value = value.clone().trim_boxed();
            }
        }
        for entity in &mut self.entities {
            trim_map(&mut entity.components);
            if let Some(instance) = &mut entity.prefab_instance {
                for override_entry in &mut instance.overrides {
                    override_entry.value = override_entry.value.clone().trim_boxed();
                }
                for add in &mut instance.added_components {
                    add.value = add.value.clone().trim_boxed();
                }
                for child in &mut instance.added_children {
                    trim_map(&mut child.components);
                }
            }
        }
        for value in self.resources.values_mut() {
            *value = value.clone().trim_boxed();
        }
        self.rebrand_level_settings_key();
        if self.format_version > SCENE_FORMAT_VERSION {
            return Err(SceneFormatError::UnsupportedVersion {
                found: self.format_version,
                supported: SCENE_FORMAT_VERSION,
            });
        }
        migrate_to_latest(self)?;
        Ok(())
    }

    /// A level written before the rename to Regia Engine keys its settings with the legacy crate
    /// name; the migrations and [`crate::level::Level`] read them by
    /// [`crate::level::LEVEL_SETTINGS_PATH`], so the key is spelled the current way here
    /// (ADR-1429). Only this key: every other type path keeps its spelling and is resolved where
    /// it meets the registry (`regia_core::brand::lookup_type_path`), which tries the written path
    /// first, so a game crate that happens to carry the legacy prefix is never renamed.
    fn rebrand_level_settings_key(&mut self) {
        let current = crate::level::LEVEL_SETTINGS_PATH;
        if self.resources.contains_key(current) {
            return;
        }
        let legacy = self
            .resources
            .keys()
            .find(|key| regia_core::brand::current_type_path(key) == current)
            .cloned();
        if let Some(value) = legacy.and_then(|key| self.resources.remove(&key)) {
            self.resources.insert(current.to_owned(), value);
        }
    }

    /// Structural validation: unique non-nil guids (top-level entities *and* prefab instances'
    /// added children, recursively), resolvable acyclic parents, and prefab-internal references
    /// that stay within the instance they belong to.
    pub fn validate(&self) -> Result<(), SceneFormatError> {
        let _span = tracing::info_span!("scene_validate", entities = self.entities.len()).entered();
        let mut by_guid: BTreeSet<Guid> = BTreeSet::new();
        // guid -> parent, for the cycle check.
        let mut parents: HashMap<Guid, Option<Guid>> = HashMap::with_capacity(self.entities.len());
        for entity in &self.entities {
            if entity.guid.is_nil() {
                return Err(SceneFormatError::Invalid(
                    "entity with nil guid (write a real guid)".into(),
                ));
            }
            if !by_guid.insert(entity.guid) {
                return Err(SceneFormatError::Invalid("duplicate entity guids".into()));
            }
            parents.insert(entity.guid, entity.parent);
        }
        // Entities whose ancestor chain is known to end at a root (each is walked once).
        let mut acyclic: HashSet<Guid> = HashSet::with_capacity(self.entities.len());

        for entity in &self.entities {
            if let Some(parent) = entity.parent {
                if parent == entity.guid {
                    return Err(SceneFormatError::Invalid(format!(
                        "entity {parent} is its own parent"
                    )));
                }
                if !by_guid.contains(&parent) {
                    return Err(SceneFormatError::Invalid(format!(
                        "entity {} references unknown parent {parent}",
                        entity.guid
                    )));
                }
                // Cycle check: walk up with a visited set, stopping at the first ancestor already
                // proven acyclic, then mark the whole walked chain acyclic (linear overall).
                let mut chain = vec![entity.guid];
                let mut seen = HashSet::from([entity.guid]);
                let mut cursor = parent;
                while !acyclic.contains(&cursor) {
                    chain.push(cursor);
                    seen.insert(cursor);
                    let Some(next) = parents.get(&cursor).copied().flatten() else {
                        break;
                    };
                    if seen.contains(&next) {
                        return Err(SceneFormatError::Invalid(format!(
                            "parent cycle through entity {next}"
                        )));
                    }
                    cursor = next;
                }
                acyclic.extend(chain);
            }
            if let Some(instance) = &entity.prefab_instance {
                validate_instance_targets(instance, entity.guid)?;
                validate_added_children(&instance.added_children, &mut by_guid, 0)?;
            }
        }
        Ok(())
    }
}

/// Rejects text whose `format_version` is newer than this build, before the full deserialise
/// (whose body may no longer match the current struct).
fn check_text_version(text: &str) -> Result<(), SceneFormatError> {
    let version = regia_core::version::peek_ron_version(text)
        .map_err(|e| SceneFormatError::Invalid(format!("cannot read format_version: {e}")))?;
    if version > SCENE_FORMAT_VERSION {
        return Err(SceneFormatError::UnsupportedVersion {
            found: version,
            supported: SCENE_FORMAT_VERSION,
        });
    }
    Ok(())
}

/// Checks an instance's `added_children` recursively (they may instantiate prefabs whose
/// `added_children` carry further records): guids non-nil and unique file-wide.
pub(crate) fn validate_added_children(
    children: &[SceneEntity],
    by_guid: &mut BTreeSet<Guid>,
    depth: u32,
) -> Result<(), SceneFormatError> {
    if depth > crate::prefab::MAX_NESTING_DEPTH {
        return Err(SceneFormatError::Invalid(format!(
            "added children nested deeper than {}",
            crate::prefab::MAX_NESTING_DEPTH
        )));
    }
    for child in children {
        if child.guid.is_nil() {
            return Err(SceneFormatError::Invalid(
                "added child with nil guid (write a real guid)".into(),
            ));
        }
        if !by_guid.insert(child.guid) {
            return Err(SceneFormatError::Invalid(format!(
                "duplicate guid {} (an added child collides with an entity)",
                child.guid
            )));
        }
        if let Some(instance) = &child.prefab_instance {
            validate_instance_targets(instance, child.guid)?;
            validate_added_children(&instance.added_children, by_guid, depth + 1)?;
        }
    }
    Ok(())
}

/// Checks that override/add/remove targets in an instance record are non-nil and distinct-ish
/// (structural sanity; existence in the prefab is checked at bake time).
pub(crate) fn validate_instance_targets(
    instance: &PrefabInstanceData,
    root: Guid,
) -> Result<(), SceneFormatError> {
    let check = |guid: Guid, what: &str| -> Result<(), SceneFormatError> {
        if guid.is_nil() {
            return Err(SceneFormatError::Invalid(format!(
                "{what} with nil target in prefab instance {root}"
            )));
        }
        Ok(())
    };
    for o in &instance.overrides {
        check(o.target, "override")?;
    }
    for a in &instance.added_components {
        check(a.target, "added component")?;
    }
    for r in &instance.removed_components {
        check(r.target, "removed component")?;
    }
    for c in &instance.removed_children {
        check(*c, "removed child")?;
    }
    for child in &instance.added_children {
        if child.guid.is_nil() {
            return Err(SceneFormatError::Invalid(format!(
                "added child with nil guid in prefab instance {root}"
            )));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Lenient optional fields (the spec A2 example writes `name:`/`prefab_instance:` bare)
// ---------------------------------------------------------------------------

/// Deserializers that accept both `Some(v)` and the bare `v` in text (RON) files.
///
/// The published format (`docs/formats/scene-files.md`) shows optional entity fields without
/// `Some(...)` (`name: "Door"`); binary files keep the standard option encoding. RON does not
/// feed bare values to `deserialize_option`, so the text path captures the raw value and
/// interprets it; the binary path is the normal `Option` encoding.
pub(crate) mod lenient_option {
    use serde::de::{
        Visitor,
        value::{
            BoolDeserializer, F32Deserializer, F64Deserializer, I64Deserializer, MapAccessDeserializer,
            SeqAccessDeserializer, StrDeserializer, StringDeserializer, U64Deserializer,
        },
    };
    use serde::{Deserialize, Deserializer};
    use std::marker::PhantomData;

    /// Reads `Option<T>` from RON (`Some(v)`, `None`, or the bare `v`) or binary data.
    ///
    /// The bare form matters because the published format shows `name: "Door"` without
    /// `Some(...)`. The visitor forwards the real (streaming) deserializer everywhere — in
    /// particular `ron::value::RawValue` fields keep capturing source text, which a
    /// materialized `ron::value::Value` cannot provide.
    pub fn deserialize<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
    where
        D: Deserializer<'de>,
        T: Deserialize<'de>,
    {
        if !deserializer.is_human_readable() {
            return Option::<T>::deserialize(deserializer);
        }
        deserializer.deserialize_any(Forwarding(PhantomData))
    }

    /// Forwards whatever `deserialize_any` produces to `T`'s own deserialization, wrapping the
    /// result in `Some`; only the explicit `None`/unit forms produce `None`.
    struct Forwarding<T>(PhantomData<T>);

    impl<'de, T: Deserialize<'de>> Visitor<'de> for Forwarding<T> {
        type Value = Option<T>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(formatter, "an optional value or the bare value")
        }

        fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
            T::deserialize(deserializer).map(Some)
        }

        fn visit_none<E>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<Self::Value, E> {
            T::deserialize(BoolDeserializer::<E>::new(v)).map(Some)
        }

        fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> {
            T::deserialize(I64Deserializer::<E>::new(v)).map(Some)
        }

        fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
            T::deserialize(U64Deserializer::<E>::new(v)).map(Some)
        }

        fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Self::Value, E> {
            T::deserialize(F64Deserializer::<E>::new(v)).map(Some)
        }

        fn visit_f32<E: serde::de::Error>(self, v: f32) -> Result<Self::Value, E> {
            T::deserialize(F32Deserializer::<E>::new(v)).map(Some)
        }

        fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
            T::deserialize(StrDeserializer::<E>::new(v)).map(Some)
        }

        fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Self::Value, E> {
            T::deserialize(StringDeserializer::<E>::new(v)).map(Some)
        }

        fn visit_seq<A: serde::de::SeqAccess<'de>>(self, seq: A) -> Result<Self::Value, A::Error> {
            T::deserialize(SeqAccessDeserializer::new(seq)).map(Some)
        }

        fn visit_map<A: serde::de::MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
            T::deserialize(MapAccessDeserializer::new(map)).map(Some)
        }
    }
}

/// Reads [`PrefabInstanceData::added_children`] with a nesting bound.
///
/// An added child may instantiate a prefab with added children of its own, so the record type
/// is recursive. `ron` stops runaway nesting with its recursion limit, but postcard has none: a
/// few dozen bytes per level let hostile postcard or cooked-scene bytes overflow the stack. A
/// list nested deeper than [`crate::prefab::MAX_NESTING_DEPTH`] could never instantiate anyway,
/// and the text format's recursion limit already stops well before it.
pub(crate) mod bounded_children {
    use std::cell::Cell;

    use serde::{Deserialize, Deserializer};

    use super::SceneEntity;

    thread_local! {
        /// `added_children` lists being read on this thread right now (outermost = 1).
        static DEPTH: Cell<u32> = const { Cell::new(0) };
    }

    /// Leaves one nesting level when its list is done (also on error).
    struct Exit;

    impl Drop for Exit {
        fn drop(&mut self) {
            DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
        }
    }

    /// Reads the list, rejecting nesting deeper than [`crate::prefab::MAX_NESTING_DEPTH`].
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<SceneEntity>, D::Error> {
        let depth = DEPTH.with(|depth| {
            let next = depth.get().saturating_add(1);
            depth.set(next);
            next
        });
        let _exit = Exit;
        if depth > crate::prefab::MAX_NESTING_DEPTH {
            return Err(serde::de::Error::custom(format!(
                "prefab instance `added_children` nested deeper than {}",
                crate::prefab::MAX_NESTING_DEPTH
            )));
        }
        Vec::<SceneEntity>::deserialize(deserializer)
    }
}

// ---------------------------------------------------------------------------
// Migrations (docs/specs/12-assets.md § 4a)
// ---------------------------------------------------------------------------

/// One registered format migration step.
#[derive(Clone, Copy, Debug)]
pub struct SceneMigration {
    /// Version the step reads.
    pub from: u32,
    /// Version the step writes.
    pub to: u32,
    /// The transform.
    pub migrate: fn(&mut SceneFile) -> Result<(), SceneFormatError>,
}

/// The built-in migration chain (also run by `regia upgrade-project`).
///
/// v2 added `LevelSettings::atmosphere` (planet preset, sun clock, clouds, sky body). The
/// `v1 → v2` step rewrites a level's settings with the field spelled out as Earth, so an old
/// map renders exactly as before and the saved file shows the new knob. v3 added
/// `LevelSettings::look` (exposure meter and colour grade) and the atmosphere's `moons` and
/// `haze`; the `v2 → v3` step spells them out at their neutral defaults the same way. v4 added
/// `LevelLook::lut` (a 3D colour LUT path); `v3 → v4` spells it out as `None`. v6 added
/// `LevelSettings::audio_medium`; `v5 → v6` spells it out with every override `None`. v7
/// added `LevelMeter::bright_threshold_cd_m2` and `bright_bias_ev` (snow compensation);
/// `v6 → v7` spells them out as 0 (off). v8 added `LevelAtmosphere::aerosol_turbidity`
/// (a per-map aerosol optical depth τ, Earth preset only); `v7 → v8` spells it out at today's
/// near-clean air so an existing Earth level keeps its exact look. v9 added
/// `LevelLook::local_exposure`; `v8 → v9` spells it out as `None` (off). v10 added
/// `LevelLook::baked_gi` (the map's `.fgiv`); `v9 → v10` spells it out as `None`. v11 added
/// `LevelLook::lightmap` (the map's `.flmap`, not written when `None`) and the saved
/// `LightmapReceiver` component (ADR-2313); `v10 → v11` re-reads the settings, so a v10 level
/// gains nothing but the version. v12 added `LevelSettings::terrain` (the terrain the level
/// names, ADR-2411, not written when `None`); `v11 → v12` re-reads the settings, so a v11 level
/// names no terrain and keeps its side files under its own name, as before. v13 added
/// `LevelLook::captures` (the map's `.fcap` local reflection capture bake, ADR-2860, not written
/// when `None`) and the saved `LocalCapture` component; `v12 → v13` re-reads the settings, so a
/// v12 level names no bake and saves as before apart from the version. v14 added
/// `LevelTerrain::sample_layout` (ADR-10079); `v13 → v14` spells it out as `VertexCentered`, so a
/// v13 level's terrain samples sit where they did.
pub fn builtin_migrations() -> &'static [SceneMigration] {
    &[
        SceneMigration {
            from: 1,
            to: 2,
            migrate: migrate_v1_to_v2,
        },
        SceneMigration {
            from: 2,
            to: 3,
            migrate: migrate_v2_to_v3,
        },
        SceneMigration {
            from: 3,
            to: 4,
            migrate: migrate_v3_to_v4,
        },
        SceneMigration {
            from: 4,
            to: 5,
            migrate: migrate_v4_to_v5,
        },
        SceneMigration {
            from: 5,
            to: 6,
            migrate: migrate_v5_to_v6,
        },
        SceneMigration {
            from: 6,
            to: 7,
            migrate: migrate_v6_to_v7,
        },
        SceneMigration {
            from: 7,
            to: 8,
            migrate: migrate_v7_to_v8,
        },
        SceneMigration {
            from: 8,
            to: 9,
            migrate: migrate_v8_to_v9,
        },
        SceneMigration {
            from: 9,
            to: 10,
            migrate: migrate_v9_to_v10,
        },
        SceneMigration {
            from: 10,
            to: 11,
            migrate: migrate_v10_to_v11,
        },
        SceneMigration {
            from: 11,
            to: 12,
            migrate: migrate_v11_to_v12,
        },
        SceneMigration {
            from: 12,
            to: 13,
            migrate: migrate_v12_to_v13,
        },
        SceneMigration {
            from: 13,
            to: 14,
            migrate: migrate_v13_to_v14,
        },
    ]
}

/// `v1 → v2`: spell out the default (Earth) atmosphere in an existing `LevelSettings`.
fn migrate_v1_to_v2(file: &mut SceneFile) -> Result<(), SceneFormatError> {
    respell_level_settings(file, 1, 2)
}

/// `v2 → v3`: spell out the default look (engine meter, neutral grade), no moons and clear air.
fn migrate_v2_to_v3(file: &mut SceneFile) -> Result<(), SceneFormatError> {
    respell_level_settings(file, 2, 3)
}

/// `v3 → v4`: spell out `look.lut` (none: the identity).
fn migrate_v3_to_v4(file: &mut SceneFile) -> Result<(), SceneFormatError> {
    respell_level_settings(file, 3, 4)
}

/// v4 → v5: sky bodies gained `fill_lux` (planet-shine). A v4 body keeps `None`, the
/// physical phase-dependent value, so every v4 level re-serialises unchanged otherwise.
fn migrate_v4_to_v5(file: &mut SceneFile) -> Result<(), SceneFormatError> {
    respell_level_settings(file, 4, 5)
}

/// v5 → v6: `LevelSettings` gained `audio_medium` (acoustic-medium overrides). A v5 level
/// spells it out with every override `None`, so it sounds as its atmosphere preset implies.
fn migrate_v5_to_v6(file: &mut SceneFile) -> Result<(), SceneFormatError> {
    respell_level_settings(file, 5, 6)
}

/// v6 → v7: `LevelMeter` gained the bright-subject (snow) compensation. A v6 level's meter
/// spells both fields out as 0, so it exposes exactly as before.
fn migrate_v6_to_v7(file: &mut SceneFile) -> Result<(), SceneFormatError> {
    respell_level_settings(file, 6, 7)
}

/// v7 → v8: `LevelAtmosphere` gained `aerosol_turbidity` (a per-map aerosol optical depth τ,
/// Earth preset only). A v7 level spells it out at today's near-clean air
/// (`TODAY_AEROSOL_TURBIDITY`), so it renders exactly as before.
fn migrate_v7_to_v8(file: &mut SceneFile) -> Result<(), SceneFormatError> {
    respell_level_settings(file, 7, 8)
}

fn migrate_v8_to_v9(file: &mut SceneFile) -> Result<(), SceneFormatError> {
    respell_level_settings(file, 8, 9)
}

/// v9 → v10: `LevelLook` gained `baked_gi` (the map's baked GI volume). A v9 level spells it
/// out as `None`: no bake, the frame lights as before.
fn migrate_v9_to_v10(file: &mut SceneFile) -> Result<(), SceneFormatError> {
    respell_level_settings(file, 9, 10)
}

/// v10 → v11 (ADR-2313): `LevelLook` gained `lightmap` (the map's `.flmap`) and
/// `regia_render::lightmap::LightmapReceiver` became a level component. A v10 level names no
/// lightmap (`None`, not written) and holds no receiver, so it loads, lights and re-saves as
/// before apart from the version.
fn migrate_v10_to_v11(file: &mut SceneFile) -> Result<(), SceneFormatError> {
    respell_level_settings(file, 10, 11)
}

/// v11 → v12 (ADR-2411): `LevelSettings` gained `terrain` (the level names its terrain). A v11
/// level names none (`None`, not written): the host keeps supplying the terrain and the side
/// files (`<level>.holes.ron`, `.weights.bin`, `.scatterpaint.bin`) keep the level's name, so it
/// loads the same terrain and re-saves as before apart from the version.
fn migrate_v11_to_v12(file: &mut SceneFile) -> Result<(), SceneFormatError> {
    respell_level_settings(file, 11, 12)
}

/// v12 → v13 (ADR-2860): `LevelLook` gained `captures` (the map's `.fcap` local reflection
/// capture bake) and `regia_render::LocalCapture` became a level component. A v12 level names no
/// bake (`None`, not written) and holds no capture, so it loads and re-saves as before apart from
/// the version.
fn migrate_v12_to_v13(file: &mut SceneFile) -> Result<(), SceneFormatError> {
    respell_level_settings(file, 12, 13)
}

/// v13 → v14 (ADR-10079): `LevelTerrain` gained `sample_layout`. A v13 level's terrain block
/// spells it out as `VertexCentered` (the only layout v13 had), so its samples sit where they did
/// and it re-saves as before apart from the version and that line. Number-only for a level that
/// names no terrain.
fn migrate_v13_to_v14(file: &mut SceneFile) -> Result<(), SceneFormatError> {
    respell_level_settings(file, 13, 14)
}

/// Re-reads a level's `LevelSettings` (missing fields take their defaults) and writes it back
/// in full, so the saved file shows every field of the new version.
fn respell_level_settings(file: &mut SceneFile, from: u32, to: u32) -> Result<(), SceneFormatError> {
    let Some(raw) = file.resources.get(crate::level::LEVEL_SETTINGS_PATH) else {
        return Ok(());
    };
    let settings =
        crate::level::LevelSettings::from_ron(raw.get_ron()).map_err(|e| SceneFormatError::Migration {
            from,
            to,
            reason: format!("LevelSettings does not parse: {e}"),
        })?;
    let text = settings.to_ron().map_err(|e| SceneFormatError::Migration {
        from,
        to,
        reason: format!("LevelSettings does not serialize: {e}"),
    })?;
    let value = ron::value::RawValue::from_ron(text.trim_end()).map_err(|e| SceneFormatError::Migration {
        from,
        to,
        reason: format!("LevelSettings re-parse: {e}"),
    })?;
    file.resources
        .insert(crate::level::LEVEL_SETTINGS_PATH.to_owned(), value.to_owned());
    Ok(())
}

/// The versions a built-in migration step leaves (`from` side), for `cargo xtask check-formats`
/// to verify a version bump ships with a migrator from the previous version.
pub fn scene_migration_sources() -> Vec<u32> {
    builtin_migrations().iter().map(|m| m.from).collect()
}

/// The serde field-list fingerprint of the scene-file format (hashed by `cargo xtask check-formats`).
///
/// The root struct *and* every nested struct are listed, so a shape change anywhere in the tree
/// changes the hash.
pub fn scene_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<SceneFile>(),
        regia_core::schema::serde_field_names::<SceneEntity>(),
        regia_core::schema::serde_field_names::<PrefabInstanceData>(),
        regia_core::schema::serde_field_names::<PrefabOverride>(),
        regia_core::schema::serde_field_names::<PrefabComponentAdd>(),
        regia_core::schema::serde_field_names::<PrefabComponentRemove>(),
        regia_core::schema::serde_field_names::<crate::level::LevelSettings>(),
        regia_core::schema::serde_field_names::<crate::level::LevelAtmosphere>(),
        regia_core::schema::serde_field_names::<crate::level::LevelTimeOfDay>(),
        regia_core::schema::serde_field_names::<crate::level::SkyBody>(),
        regia_core::schema::serde_field_names::<crate::level::LevelHaze>(),
        regia_core::schema::serde_field_names::<crate::level::LevelLook>(),
        regia_core::schema::serde_field_names::<crate::level::LevelMeter>(),
        regia_core::schema::serde_field_names::<crate::level::LevelGrade>(),
        regia_core::schema::serde_field_names::<crate::level::LevelLocalExposure>(),
        regia_core::schema::serde_field_names::<crate::level::LevelBakedGi>(),
        regia_core::schema::serde_field_names::<crate::level::LevelBakedGiSettings>(),
        regia_core::schema::serde_field_names::<crate::level::LevelLightmap>(),
        regia_core::schema::serde_field_names::<crate::level::LevelCaptures>(),
        regia_core::schema::serde_field_names::<crate::level::LevelTerrain>(),
        regia_core::schema::serde_field_names::<crate::level::LevelAudioMedium>(),
        regia_core::schema::serde_variant_names::<crate::level::AtmospherePreset>(),
        regia_core::schema::serde_variant_names::<crate::level::TerrainSampleLayout>(),
    ])
}

/// Runs every migration step needed to bring `file` up to [`SCENE_FORMAT_VERSION`].
/// Errors on gaps in the chain (a version with no registered step).
pub fn migrate_to_latest(file: &mut SceneFile) -> Result<u32, SceneFormatError> {
    run_migration_chain(file, builtin_migrations(), SCENE_FORMAT_VERSION)
}

/// Steps `file` through `chain` up to `target`, stamping each step's target version. Exposed
/// (crate-private) so the stepping loop, version stamping and gap errors are testable before
/// real migrations exist.
fn run_migration_chain(
    file: &mut SceneFile,
    chain: &[SceneMigration],
    target: u32,
) -> Result<u32, SceneFormatError> {
    let mut version = file.format_version;
    let mut guard = 0;
    while version < target {
        let Some(step) = chain.iter().find(|m| m.from == version) else {
            return Err(SceneFormatError::Migration {
                from: version,
                to: target,
                reason: "no migration registered for this version".into(),
            });
        };
        // A step must advance the version, otherwise an identity entry shadows a later step from
        // the same version and loops the chain.
        if step.to <= version {
            return Err(SceneFormatError::Migration {
                from: version,
                to: target,
                reason: format!(
                    "migration step v{} -> v{} does not advance the version",
                    step.from, step.to
                ),
            });
        }
        (step.migrate)(file)?;
        version = step.to;
        file.format_version = version;
        guard += 1;
        if guard > 32 {
            return Err(SceneFormatError::Migration {
                from: file.format_version,
                to: target,
                reason: "migration chain did not converge".into(),
            });
        }
    }
    Ok(version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deeply_nested_added_children_are_rejected() {
        let nest = |levels: u32| {
            let mut record = SceneEntity {
                guid: Guid::new_random(),
                name: None,
                parent: None,
                components: BTreeMap::new(),
                prefab_instance: None,
            };
            for _ in 0..levels {
                let mut instance = PrefabInstanceData::plain(SceneAssetRef(Guid::new_random()));
                instance.added_children = vec![record];
                record = SceneEntity {
                    guid: Guid::new_random(),
                    name: None,
                    parent: None,
                    components: BTreeMap::new(),
                    prefab_instance: Some(instance),
                };
            }
            record
        };
        let shallow = [nest(4)];
        assert!(validate_added_children(&shallow, &mut BTreeSet::new(), 0).is_ok());
        let deep = [nest(crate::prefab::MAX_NESTING_DEPTH + 8)];
        assert!(validate_added_children(&deep, &mut BTreeSet::new(), 0).is_err());
    }

    fn spec_example_text() -> String {
        let door_guid = Guid::from_u128(0x8f1c);
        let light_guid = Guid::from_u128(0x0f1e);
        let prefab_ref = Guid::from_u128(0x44fe);
        format!(
            r#"
SceneFile(
    format_version: 1,
    entities: [
        (
            guid: "{door_guid}",
            name: "Door",
            components: {{
                "regia_scene::transform::Transform": (translation: (0.0, 0.0, -3.0)),
            }},
            prefab_instance: (
                prefab: "asset:{prefab_ref}",
                overrides: [(target: "{light_guid}", component: "regia_scene::components::Tags", path: ".0", value: (["door"]))],
            ),
        ),
    ],
)
"#
        )
    }

    #[test]
    fn parses_and_round_trips_the_spec_example() {
        let door_guid = Guid::from_u128(0x8f1c);
        let level_guid = Guid::from_u128(0x2ab0);
        let light_guid = Guid::from_u128(0x0f1e);
        let prefab_ref = Guid::from_u128(0x44fe);
        let text = format!(
            r#"
SceneFile(
    format_version: 1,
    entities: [
        (
            guid: "{door_guid}",
            name: "Door",
            parent: Some("{level_guid}"),
            components: {{
                "regia_scene::transform::Transform": (translation: (0.0, 0.0, -3.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (1.0, 1.0, 1.0)),
            }},
        ),
        (
            guid: "{level_guid}",
            name: "Level",
            components: {{}},
            prefab_instance: (
                prefab: "asset:{prefab_ref}",
                overrides: [
                    (
                        target: "{light_guid}",
                        component: "regia_scene::components::Tags",
                        path: ".0",
                        value: (["door", "interactive"]),
                    ),
                ],
                added_components: [],
                removed_components: [],
                added_children: [],
                removed_children: [],
            ),
        ),
    ],
    resources: {{}},
)
"#
        );

        let file = SceneFile::from_ron_str(&text).expect("parse");
        assert_eq!(file.format_version, SCENE_FORMAT_VERSION);
        assert_eq!(file.entities.len(), 2);
        let door = file
            .entities
            .iter()
            .find(|e| e.name.as_deref() == Some("Door"))
            .expect("door");
        assert_eq!(door.parent, Some(level_guid));
        assert!(door.components.contains_key("regia_scene::transform::Transform"));
        let level = file
            .entities
            .iter()
            .find(|e| e.guid == level_guid)
            .expect("level");
        let instance = level.prefab_instance.as_ref().expect("instance");
        assert_eq!(instance.prefab, SceneAssetRef(prefab_ref));
        assert_eq!(instance.overrides.len(), 1);
        assert_eq!(instance.overrides[0].target, light_guid);

        let rewritten = file.to_ron_string().expect("write");
        let reparsed = SceneFile::from_ron_str(&rewritten).expect("reparse");
        assert_eq!(reparsed, file);
        // Canonical output is stable: writing again is byte-identical.
        assert_eq!(reparsed.to_ron_string().expect("write"), rewritten);
    }

    #[test]
    fn accepts_bare_struct_without_name() {
        let text = "(format_version: 1, entities: [], resources: {})";
        let file = SceneFile::from_ron_str(text).expect("bare form parses");
        assert!(file.entities.is_empty());
    }

    #[test]
    fn rejects_newer_format_version() {
        let text = "(format_version: 99, entities: [], resources: {})";
        let err = SceneFile::from_ron_str(text).expect_err("must reject");
        assert!(matches!(
            err,
            SceneFormatError::UnsupportedVersion {
                found: 99,
                supported: SCENE_FORMAT_VERSION
            }
        ));
    }

    #[test]
    fn rejects_newer_version_even_when_body_changed() {
        // A newer file whose body no longer matches the v1 struct is a *version* error, not a RON
        // error (the version is peeked before the full deserialise).
        let text = "(format_version: 99, entities: [(future_field: 1)], resources: {})";
        let err = SceneFile::from_ron_str(text).expect_err("must reject");
        assert!(
            matches!(
                err,
                SceneFormatError::UnsupportedVersion {
                    found: 99,
                    supported: SCENE_FORMAT_VERSION
                }
            ),
            "{err}"
        );
    }

    #[test]
    fn current_version_is_already_latest() {
        let mut file = SceneFile {
            format_version: 1,
            ..SceneFile::default()
        };
        let to = migrate_to_latest(&mut file).expect("migrate");
        assert_eq!(to, SCENE_FORMAT_VERSION);
        // Every registered step advances the version (no identity step shadows a later one).
        assert!(builtin_migrations().iter().all(|m| m.to > m.from));
        assert_eq!(builtin_migrations()[0].from, 1, "v1 levels have a migrator");
    }

    /// A v1 level with settings but no `atmosphere` loads as Earth and is stamped v2.
    #[test]
    fn v1_level_migrates_to_an_earth_atmosphere() {
        let text = r#"SceneFile(
    format_version: 1,
    entities: [],
    resources: {
        "regia_scene::level::LevelSettings": (gravity: (0.0, -3.7, 0.0)),
    },
)"#;
        let file = SceneFile::from_ron_str(text).expect("v1 loads");
        assert_eq!(file.format_version, SCENE_FORMAT_VERSION);
        let raw = file
            .resources
            .get(crate::level::LEVEL_SETTINGS_PATH)
            .expect("settings kept");
        assert!(raw.get_ron().contains("atmosphere"), "{}", raw.get_ron());
        let settings = crate::level::LevelSettings::from_ron(raw.get_ron()).expect("parse");
        assert_eq!(settings.gravity.y, -3.7);
        assert_eq!(settings.atmosphere.preset, crate::level::AtmospherePreset::Earth);
        assert!(settings.atmosphere.sky_body.is_none());
        // v2 -> v3 spelled out the neutral look as well.
        assert!(raw.get_ron().contains("look"), "{}", raw.get_ron());
        assert_eq!(settings.look, crate::level::LevelLook::default());
    }

    /// A v3 level (look, no LUT) migrates to v4 with `lut: None`, keeping its look.
    #[test]
    fn a_v4_level_migrates_to_v5_with_the_physical_planet_shine() {
        let text = r#"SceneFile(
    format_version: 4,
    entities: [],
    resources: {
        "regia_scene::level::LevelSettings": (atmosphere: (preset: Airless, sky_body: Some((angular_diameter_deg: 12.0)))),
    },
)"#;
        let file = SceneFile::from_ron_str(text).expect("v4 loads");
        assert_eq!(file.format_version, SCENE_FORMAT_VERSION);
        let raw = file
            .resources
            .get(crate::level::LEVEL_SETTINGS_PATH)
            .expect("settings kept");
        let settings = crate::level::LevelSettings::from_ron(raw.get_ron()).expect("parse");
        let body = settings.atmosphere.sky_body.expect("sky body kept");
        assert_eq!(body.angular_diameter_deg, 12.0);
        assert_eq!(body.fill_lux, None);
        assert!(raw.get_ron().contains("fill_lux"), "{}", raw.get_ron());
    }

    /// A v5 level migrates to v6 with the audio-medium overrides spelled out as `None`.
    #[test]
    fn a_v5_level_migrates_to_v6_with_no_audio_medium_overrides() {
        let text = r#"SceneFile(
    format_version: 5,
    entities: [],
    resources: {
        "regia_scene::level::LevelSettings": (atmosphere: (preset: Mars)),
    },
)"#;
        let file = SceneFile::from_ron_str(text).expect("v5 loads");
        assert_eq!(file.format_version, SCENE_FORMAT_VERSION);
        let raw = file
            .resources
            .get(crate::level::LEVEL_SETTINGS_PATH)
            .expect("settings kept");
        let settings = crate::level::LevelSettings::from_ron(raw.get_ron()).expect("parse");
        assert_eq!(settings.atmosphere.preset, crate::level::AtmospherePreset::Mars);
        assert_eq!(settings.audio_medium, crate::level::LevelAudioMedium::default());
        assert!(raw.get_ron().contains("audio_medium"), "{}", raw.get_ron());
    }

    /// A v6 level's meter migrates to v7 with the snow compensation spelled out as off.
    #[test]
    fn a_v6_level_migrates_to_v7_with_the_snow_compensation_off() {
        let text = r#"SceneFile(
    format_version: 6,
    entities: [],
    resources: {
        "regia_scene::level::LevelSettings": (look: (meter: Some((low_percentile: 0.45, center_weight: 0.3)))),
    },
)"#;
        let file = SceneFile::from_ron_str(text).expect("v6 loads");
        assert_eq!(file.format_version, SCENE_FORMAT_VERSION);
        let raw = file
            .resources
            .get(crate::level::LEVEL_SETTINGS_PATH)
            .expect("settings kept");
        let settings = crate::level::LevelSettings::from_ron(raw.get_ron()).expect("parse");
        let meter = settings.look.meter.expect("meter kept");
        assert_eq!(meter.low_percentile, 0.45);
        assert_eq!(meter.center_weight, 0.3);
        assert_eq!(meter.bright_threshold_cd_m2, 0.0);
        assert_eq!(meter.bright_bias_ev, 0.0);
        assert!(
            raw.get_ron().contains("bright_threshold_cd_m2"),
            "{}",
            raw.get_ron()
        );
    }

    /// A v7 level's atmosphere migrates to v8 with `aerosol_turbidity` spelled out at today's
    /// near-clean air, so an existing Earth level renders exactly as before.
    #[test]
    fn a_v7_level_migrates_to_v8_with_todays_aerosol_turbidity() {
        let text = r#"SceneFile(
    format_version: 7,
    entities: [],
    resources: {
        "regia_scene::level::LevelSettings": (atmosphere: (preset: Earth, cloud_coverage: 0.3)),
    },
)"#;
        let file = SceneFile::from_ron_str(text).expect("v7 loads");
        assert_eq!(file.format_version, SCENE_FORMAT_VERSION);
        let raw = file
            .resources
            .get(crate::level::LEVEL_SETTINGS_PATH)
            .expect("settings kept");
        let settings = crate::level::LevelSettings::from_ron(raw.get_ron()).expect("parse");
        assert_eq!(settings.atmosphere.preset, crate::level::AtmospherePreset::Earth);
        assert_eq!(settings.atmosphere.cloud_coverage, 0.3);
        assert_eq!(
            settings.atmosphere.aerosol_turbidity,
            crate::level::TODAY_AEROSOL_TURBIDITY
        );
        assert!(raw.get_ron().contains("aerosol_turbidity"), "{}", raw.get_ron());
    }

    /// A v8 level's look migrates to v9 with local exposure spelled out as off.
    #[test]
    fn a_v8_level_migrates_to_v9_with_local_exposure_off() {
        let text = r#"SceneFile(
    format_version: 8,
    entities: [],
    resources: {
        "regia_scene::level::LevelSettings": (look: (exposure_bias_ev: 0.25)),
    },
)"#;
        let file = SceneFile::from_ron_str(text).expect("v8 loads");
        assert_eq!(file.format_version, SCENE_FORMAT_VERSION);
        let raw = file
            .resources
            .get(crate::level::LEVEL_SETTINGS_PATH)
            .expect("settings kept");
        let settings = crate::level::LevelSettings::from_ron(raw.get_ron()).expect("parse");
        assert_eq!(settings.look.exposure_bias_ev, 0.25);
        assert_eq!(settings.look.local_exposure, None);
        assert!(raw.get_ron().contains("local_exposure"), "{}", raw.get_ron());
    }

    /// A v9 level's look migrates to v10 with `baked_gi` spelled out as `None`.
    #[test]
    fn a_v9_level_migrates_to_v10_with_no_baked_gi() {
        let text = r#"SceneFile(
    format_version: 9,
    entities: [],
    resources: {
        "regia_scene::level::LevelSettings": (look: (exposure_bias_ev: 0.25)),
    },
)"#;
        let file = SceneFile::from_ron_str(text).expect("v9 loads");
        assert_eq!(file.format_version, SCENE_FORMAT_VERSION);
        let raw = file
            .resources
            .get(crate::level::LEVEL_SETTINGS_PATH)
            .expect("settings kept");
        let settings = crate::level::LevelSettings::from_ron(raw.get_ron()).expect("parse");
        assert_eq!(settings.look.exposure_bias_ev, 0.25);
        assert_eq!(settings.look.baked_gi, None);
        assert!(raw.get_ron().contains("baked_gi"), "{}", raw.get_ron());
    }

    /// A v10 level naming a bake round-trips its volume path and overrides.
    #[test]
    fn a_v10_level_keeps_its_baked_gi() {
        let text = r#"SceneFile(
    format_version: 10,
    entities: [],
    resources: {
        "regia_scene::level::LevelSettings": (look: (baked_gi: Some((volume: "maps/a.fgiv", settings: (sky_scale: Some(0.5)))))),
    },
)"#;
        let file = SceneFile::from_ron_str(text).expect("v10 loads");
        let raw = file
            .resources
            .get(crate::level::LEVEL_SETTINGS_PATH)
            .expect("settings kept");
        let settings = crate::level::LevelSettings::from_ron(raw.get_ron()).expect("parse");
        let bake = settings.look.baked_gi.expect("baked_gi kept");
        assert_eq!(bake.volume, "maps/a.fgiv");
        assert_eq!(bake.settings.sky_scale, Some(0.5));
        assert_eq!(bake.settings.intensity, None);
    }

    /// A v10 level migrates to v11 with no lightmap (ADR-2313), and its settings text is the one a
    /// v10 writer wrote: `lightmap: None` is not written.
    #[test]
    fn a_v10_level_migrates_to_v11_with_no_lightmap() {
        let text = r#"SceneFile(
    format_version: 10,
    entities: [],
    resources: {
        "regia_scene::level::LevelSettings": (look: (exposure_bias_ev: 0.25, baked_gi: Some((volume: "maps/a.fgiv")))),
    },
)"#;
        let file = SceneFile::from_ron_str(text).expect("v10 loads");
        assert_eq!(file.format_version, SCENE_FORMAT_VERSION);
        const { assert!(SCENE_FORMAT_VERSION >= 11, "v11 added the lightmap") };
        let raw = file
            .resources
            .get(crate::level::LEVEL_SETTINGS_PATH)
            .expect("settings kept");
        let settings = crate::level::LevelSettings::from_ron(raw.get_ron()).expect("parse");
        assert_eq!(settings.look.lightmap, None);
        assert_eq!(settings.look.exposure_bias_ev, 0.25);
        assert!(settings.look.baked_gi.is_some());
        assert!(!raw.get_ron().contains("lightmap"), "{}", raw.get_ron());
    }

    /// A level without a lightmap saves byte for byte as a v10 writer saved it, apart from the
    /// version line (ADR-2313); a level naming one keeps the file and overrides.
    #[test]
    fn a_level_without_a_lightmap_saves_as_before_and_one_with_keeps_it() {
        let v10 = r#"SceneFile(
    format_version: 10,
    entities: [],
    resources: {
        "regia_scene::level::LevelSettings": (look: (exposure_bias_ev: 0.25)),
    },
)"#;
        let level = crate::level::Level::from_ron_str(crate::level::LevelId::default(), v10).expect("loads");
        let saved = level.to_ron_string().expect("saves");
        let mut as_v10 = level.clone();
        as_v10.scene.format_version = 10;
        let old = as_v10.to_ron_string().expect("saves");
        assert_eq!(
            saved.replacen(
                &format!("format_version: {SCENE_FORMAT_VERSION}"),
                "format_version: 10",
                1
            ),
            old,
            "only the version differs"
        );
        assert!(!saved.contains("lightmap"), "{saved}");

        let mut with = level;
        with.settings.look.lightmap = Some(crate::level::LevelLightmap {
            file: "maps/yard.flmap".into(),
            enabled: Some(false),
            intensity: Some(0.5),
        });
        let text = with.to_ron_string().expect("saves");
        let back = crate::level::Level::from_ron_str(with.id, &text).expect("reloads");
        assert_eq!(back.settings.look.lightmap, with.settings.look.lightmap);
        assert_eq!(
            back.to_ron_string().expect("re-saves"),
            text,
            "a re-save is byte-identical"
        );
    }

    #[test]
    fn a_v3_level_migrates_to_v4_without_a_lut() {
        let text = r#"SceneFile(
    format_version: 3,
    entities: [],
    resources: {
        "regia_scene::level::LevelSettings": (look: (exposure_bias_ev: 0.5)),
    },
)"#;
        let file = SceneFile::from_ron_str(text).expect("v3 loads");
        assert_eq!(file.format_version, SCENE_FORMAT_VERSION);
        let raw = file
            .resources
            .get(crate::level::LEVEL_SETTINGS_PATH)
            .expect("settings kept");
        let settings = crate::level::LevelSettings::from_ron(raw.get_ron()).expect("parse");
        assert_eq!(settings.look.exposure_bias_ev, 0.5);
        assert_eq!(settings.look.lut, None);
        assert!(raw.get_ron().contains("lut"), "{}", raw.get_ron());
    }

    /// A v2 level (atmosphere, no look) migrates to v3 with the engine look and clear air,
    /// keeping its atmosphere.
    #[test]
    fn a_v2_level_migrates_to_v3_with_the_default_look() {
        let text = r#"SceneFile(
    format_version: 2,
    entities: [],
    resources: {
        "regia_scene::level::LevelSettings": (atmosphere: (preset: Mars)),
    },
)"#;
        let file = SceneFile::from_ron_str(text).expect("v2 loads");
        assert_eq!(file.format_version, SCENE_FORMAT_VERSION);
        let raw = file
            .resources
            .get(crate::level::LEVEL_SETTINGS_PATH)
            .expect("settings kept");
        let settings = crate::level::LevelSettings::from_ron(raw.get_ron()).expect("parse");
        assert_eq!(settings.atmosphere.preset, crate::level::AtmospherePreset::Mars);
        assert_eq!(settings.look, crate::level::LevelLook::default());
        assert!(settings.atmosphere.moons.is_empty());
        assert_eq!(settings.atmosphere.haze.density, 0.0);
        assert!(raw.get_ron().contains("haze") && raw.get_ron().contains("look"));
    }

    /// An identity step would shadow a later `v1 → v2` step; the chain rejects non-advancing steps.
    #[test]
    fn migration_chain_rejects_a_non_advancing_step() {
        let chain = [SceneMigration {
            from: 1,
            to: 1,
            migrate: |_| Ok(()),
        }];
        let mut file = SceneFile {
            format_version: 1,
            ..SceneFile::default()
        };
        let err = run_migration_chain(&mut file, &chain, 2).expect_err("identity must be rejected");
        assert!(
            matches!(err, SceneFormatError::Migration { ref reason, .. } if reason.contains("does not advance")),
            "{err}"
        );
    }

    /// The stepping loop itself, driven with a fake chain: versions are stamped per step, a gap
    /// in the chain is an error, and the target version is honoured.
    #[test]
    fn migration_chain_steps_and_reports_gaps() {
        fn bump(file: &mut SceneFile) -> Result<(), SceneFormatError> {
            file.entities.push(SceneEntity {
                guid: Guid::from_u128(file.entities.len() as u128 + 1),
                ..SceneEntity::default()
            });
            Ok(())
        }
        let chain = [
            SceneMigration {
                from: 0,
                to: 2,
                migrate: bump,
            },
            SceneMigration {
                from: 2,
                to: 5,
                migrate: bump,
            },
        ];
        let mut file = SceneFile {
            format_version: 0,
            ..SceneFile::default()
        };
        let to = run_migration_chain(&mut file, &chain, 5).expect("chain");
        assert_eq!(to, 5);
        assert_eq!(file.format_version, 5);
        assert_eq!(file.entities.len(), 2, "both steps ran");

        // A version with no registered step is an error.
        let mut file = SceneFile {
            format_version: 3,
            ..SceneFile::default()
        };
        let err = run_migration_chain(&mut file, &chain, 5).expect_err("gap");
        assert!(
            matches!(err, SceneFormatError::Migration { from: 3, .. }),
            "{err}"
        );
    }

    #[test]
    fn postcard_round_trip() {
        let file = SceneFile::from_ron_str(&spec_example_text()).expect("parse");
        let bytes = file.to_postcard().expect("postcard");
        let back = SceneFile::from_postcard(&bytes).expect("unpostcard");
        assert_eq!(back, file);
    }

    #[test]
    fn asset_ref_round_trip() {
        let guid = Guid::new_random();
        let r = SceneAssetRef(guid);
        assert_eq!(r.to_string(), format!("asset:{guid}"));
        assert_eq!(format!("{r}").parse::<SceneAssetRef>().ok(), Some(r));
        assert_eq!(format!("{guid}").parse::<SceneAssetRef>().ok(), Some(r));
        assert!("asset:nope".parse::<SceneAssetRef>().is_err());
    }

    #[test]
    fn validation_rejects_bad_structure() {
        let base = SceneEntity {
            guid: Guid::from_u128(1),
            ..SceneEntity::default()
        };

        let mut dup = SceneFile::new();
        dup.entities = vec![base.clone(), base.clone()];
        assert!(dup.validate().is_err(), "duplicate guids");

        let mut orphan = SceneFile::new();
        orphan.entities = vec![SceneEntity {
            parent: Some(Guid::from_u128(77)),
            ..base.clone()
        }];
        assert!(orphan.validate().is_err(), "unknown parent");

        let mut cycle = SceneFile::new();
        cycle.entities = vec![
            SceneEntity {
                guid: Guid::from_u128(1),
                parent: Some(Guid::from_u128(2)),
                ..SceneEntity::default()
            },
            SceneEntity {
                guid: Guid::from_u128(2),
                parent: Some(Guid::from_u128(1)),
                ..SceneEntity::default()
            },
        ];
        assert!(cycle.validate().is_err(), "parent cycle");

        let mut self_parent = SceneFile::new();
        self_parent.entities = vec![SceneEntity {
            guid: Guid::from_u128(3),
            parent: Some(Guid::from_u128(3)),
            ..SceneEntity::default()
        }];
        assert!(self_parent.validate().is_err(), "self parent");

        let mut nil = SceneFile::new();
        nil.entities = vec![SceneEntity {
            guid: Guid::NIL,
            ..SceneEntity::default()
        }];
        assert!(nil.validate().is_err(), "nil guid");
    }

    /// The pre-split loader: one whole-document `ron` parse (the reference for the split path).
    fn from_whole_ron_str(text: &str) -> Result<SceneFile, SceneFormatError> {
        check_text_version(text)?;
        let mut file: SceneFile = ron::from_str(text)?;
        file.finish_load()?;
        Ok(file)
    }

    /// The split loader reads exactly what one whole-document parse reads, including comments,
    /// strings with brackets, named records, trailing commas and nested prefab records.
    #[test]
    fn split_parse_matches_whole_document_parse() {
        let a = Guid::from_u128(0xa1);
        let b = Guid::from_u128(0xb2);
        let tricky = format!(
            r##"// header comment with entities: [ ]
SceneFile(
    format_version: 5,
    /* entities: [ block ] */
    entities: [
        // leading comment ]
        SceneEntity(guid: "{a}", name: "A ] [ \" (", components: {{
            "regia_scene::components::Tags": (["]", "/* not a comment */", "r#\"x\"#"]),
            "game::Chars": (c: ']', q: '\'', raw: r#"]"#),
        }}), // trailing ]
        (
            guid: "{b}",
            parent: "{a}",
            components: {{ "game::Nested": (a: [(b: {{ "k": [1.5, -2.0e3] }})]) }},
        ),
    ],
    resources: {{ "game::Res": (x: 1.0) }},
)
"##
        );
        let mut texts = vec![
            spec_example_text(),
            tricky,
            include_str!("../tests/fixtures/level.example.ron").to_owned(),
            include_str!("../tests/fixtures/prefab.example.ron").to_owned(),
            "(format_version: 5)".to_owned(),
            "#![enable(implicit_some)]\n(format_version: 5, entities: [])".to_owned(),
        ];
        let mut big = SceneFile::new();
        for index in 0..50u32 {
            let mut components = BTreeMap::new();
            components.insert(
                "regia_scene::transform::Transform".to_owned(),
                ron::value::RawValue::from_ron(&format!("(translation: ({index}.0, 0.0, 1.5))"))
                    .expect("ron")
                    .to_owned(),
            );
            big.entities.push(SceneEntity {
                guid: Guid::from_u128(u128::from(index) + 1),
                name: Some(format!("E{index}")),
                parent: (index > 0).then(|| Guid::from_u128(u128::from(index))),
                components,
                prefab_instance: None,
            });
        }
        texts.push(big.to_ron_string().expect("write"));

        for text in &texts {
            let split = SceneFile::from_ron_str(text).expect("split path parses");
            let whole = from_whole_ron_str(text).expect("whole path parses");
            assert_eq!(split, whole, "{text}");
            for (left, right) in split.entities.iter().zip(&whole.entities) {
                for (key, value) in &left.components {
                    assert_eq!(
                        value.get_ron(),
                        right.components[key].get_ron(),
                        "raw text of {key}"
                    );
                }
            }
        }
    }

    /// A broken record reports the same error and position as a whole-document parse.
    #[test]
    fn split_parse_errors_point_at_the_whole_text() {
        let a = Guid::from_u128(0xa1);
        for broken in [
            // Type error: a number where a guid string belongs.
            format!("(format_version: 5, entities: [\n    (guid: \"{a}\"),\n    (guid: 7),\n])"),
            // Type error on the first line of the file.
            "(format_version: 5, entities: [(guid: 7)])".to_owned(),
            // Syntax error inside a record.
            format!("(format_version: 5, entities: [\n  (guid: \"{a}\", name: ),\n])"),
        ] {
            let SceneFormatError::RonParse(split) =
                SceneFile::from_ron_str(&broken).expect_err("broken record")
            else {
                panic!("expected a positioned RON error for {broken}");
            };
            let whole = ron::from_str::<SceneFile>(&broken).expect_err("whole parse fails too");
            assert_eq!(split.code, whole.code, "{broken}");
            assert_eq!(split.span, whole.span, "{broken}");
        }
    }

    /// A long parent chain validates in linear time (the old cycle check rescanned the entity
    /// list for every ancestor) and a cycle deep in a chain is still found.
    #[test]
    fn validate_handles_deep_chains_and_finds_late_cycles() {
        let mut chain = SceneFile::new();
        for index in 0..20_000u32 {
            chain.entities.push(SceneEntity {
                guid: Guid::from_u128(u128::from(index) + 1),
                parent: (index > 0).then(|| Guid::from_u128(u128::from(index))),
                ..SceneEntity::default()
            });
        }
        let start = std::time::Instant::now();
        chain.validate().expect("a chain is valid");
        assert!(
            start.elapsed().as_secs_f64() < 2.0,
            "validating a 20,000-deep chain took {:?}",
            start.elapsed()
        );

        // Close the chain's top into a loop: 1 -> 20000.
        chain.entities[0].parent = Some(Guid::from_u128(20_000));
        assert!(chain.validate().is_err(), "cycle through the whole chain");

        // A three-cycle hanging off an acyclic root, reached from an entity listed first.
        let id = |n: u128| Guid::from_u128(n);
        let mut lasso = SceneFile::new();
        for (guid, parent) in [(1, Some(2)), (2, Some(3)), (3, Some(4)), (4, Some(2)), (5, None)] {
            lasso.entities.push(SceneEntity {
                guid: id(guid),
                parent: parent.map(id),
                ..SceneEntity::default()
            });
        }
        assert!(lasso.validate().is_err(), "2 -> 3 -> 4 -> 2");
    }

    /// ADR-2411: a v11 level migrates to v12 naming no terrain, so it re-saves byte for byte as
    /// before apart from the version, and a level that names a terrain keeps every field.
    #[test]
    fn a_v11_level_migrates_naming_no_terrain_and_a_v12_level_keeps_its_terrain() {
        let v11 = r#"SceneFile(
    format_version: 11,
    entities: [],
    resources: {
        "regia_scene::level::LevelSettings": (look: (exposure_bias_ev: 0.5)),
    },
)"#;
        let level =
            crate::level::Level::from_ron_str(crate::level::LevelId::default(), v11).expect("v11 loads");
        assert_eq!(level.scene.format_version, SCENE_FORMAT_VERSION);
        assert_eq!(level.settings.terrain, None);
        let saved = level.to_ron_string().expect("saves");
        assert!(!saved.contains("terrain"), "{saved}");
        let mut as_v11 = level.clone();
        as_v11.scene.format_version = 11;
        assert_eq!(
            saved.replacen(
                &format!("format_version: {SCENE_FORMAT_VERSION}"),
                "format_version: 11",
                1
            ),
            as_v11.to_ron_string().expect("saves"),
            "only the version differs"
        );

        let mut with = level;
        with.settings.terrain = Some(crate::level::LevelTerrain {
            layers: "maps/arena/terrain.layers.ron".into(),
            map: "maps/arena".into(),
            world_width_m: Some(512.0),
            height_scale_m: 120.0,
            height_offset_m: -4.0,
            ..crate::level::LevelTerrain::default()
        });
        let text = with.to_ron_string().expect("saves");
        assert!(text.contains("maps/arena/terrain.layers.ron"), "{text}");
        let back = crate::level::Level::from_ron_str(crate::level::LevelId::default(), &text).expect("loads");
        assert_eq!(back.settings.terrain, with.settings.terrain);
        assert_eq!(back.to_ron_string().expect("saves"), text, "canonical");
    }

    /// ADR-10079: a v13 level that names a terrain migrates to v14 with `sample_layout:
    /// VertexCentered` (the layout v13 had) and every other field kept; migrating twice gives
    /// the same bytes; a v14 level's `CellCentered` survives a load and a save; a v15 level is
    /// refused with the version error.
    #[test]
    fn a_v13_terrain_migrates_vertex_centered_and_a_v14_cell_centered_terrain_round_trips() {
        let v13 = r#"SceneFile(
    format_version: 13,
    entities: [],
    resources: {
        "regia_scene::level::LevelSettings": (terrain: Some((
            layers: "maps/arena/terrain.layers.ron",
            map: "maps/arena",
            world_width_m: Some(1280.0),
            height_scale_m: 300.0,
            height_offset_m: -12.0,
        ))),
    },
)"#;
        let level =
            crate::level::Level::from_ron_str(crate::level::LevelId::default(), v13).expect("v13 loads");
        assert_eq!(level.scene.format_version, 14);
        let terrain = level.settings.terrain.clone().expect("terrain kept");
        assert_eq!(
            terrain.sample_layout,
            crate::level::TerrainSampleLayout::VertexCentered
        );
        assert_eq!(
            (
                terrain.world_width_m,
                terrain.height_scale_m,
                terrain.height_offset_m
            ),
            (Some(1280.0), 300.0, -12.0)
        );
        let once = level.to_ron_string().expect("saves");
        assert!(once.contains("sample_layout: VertexCentered"), "{once}");
        let twice = crate::level::Level::from_ron_str(crate::level::LevelId::default(), &once)
            .expect("v14 loads")
            .to_ron_string()
            .expect("saves");
        assert_eq!(once, twice, "migration is idempotent");

        let v14 = once.replace("sample_layout: VertexCentered", "sample_layout: CellCentered");
        let cell =
            crate::level::Level::from_ron_str(crate::level::LevelId::default(), &v14).expect("v14 loads");
        assert_eq!(
            cell.settings.terrain.as_ref().map(|t| t.sample_layout),
            Some(crate::level::TerrainSampleLayout::CellCentered)
        );
        assert_eq!(cell.to_ron_string().expect("saves"), v14, "canonical");

        let v15 = v14.replacen("format_version: 14", "format_version: 15", 1);
        let err = crate::level::Level::from_ron_str(crate::level::LevelId::default(), &v15)
            .expect_err("a newer version is refused");
        assert!(
            err.to_string()
                .contains("scene file format version 15 is newer than supported version 14"),
            "{err}"
        );
    }

    #[test]
    fn level_terrain_validation_names_the_field() {
        use crate::level::LevelTerrain;
        assert!(LevelTerrain::default().validate().is_ok());
        let bad = |t: LevelTerrain| t.validate().expect_err("refused");
        assert!(
            bad(LevelTerrain {
                layers: "../x.terrain.layers.ron".into(),
                ..LevelTerrain::default()
            })
            .contains("terrain.layers")
        );
        assert!(
            bad(LevelTerrain {
                layers: "maps/x.ron".into(),
                ..LevelTerrain::default()
            })
            .contains(".terrain.layers.ron")
        );
        // The cook convention's plain `terrain.layers.ron` and a named one are both layer files.
        for layers in ["maps/a/terrain.layers.ron", "maps/a.terrain.layers.ron"] {
            let ok = LevelTerrain {
                layers: layers.into(),
                ..LevelTerrain::default()
            };
            assert!(ok.validate().is_ok(), "{layers}");
        }
        assert!(
            bad(LevelTerrain {
                layers: "maps/a/xterrain.layers.ron".into(),
                ..LevelTerrain::default()
            })
            .contains(".terrain.layers.ron")
        );
        assert!(
            bad(LevelTerrain {
                map: "/abs".into(),
                ..LevelTerrain::default()
            })
            .contains("terrain.map")
        );
        assert!(
            bad(LevelTerrain {
                map: "C:maps/x".into(),
                ..LevelTerrain::default()
            })
            .contains("terrain.map")
        );
        assert!(
            bad(LevelTerrain {
                world_width_m: Some(0.0),
                ..LevelTerrain::default()
            })
            .contains("world_width_m")
        );
        assert!(
            bad(LevelTerrain {
                height_scale_m: f32::NAN,
                ..LevelTerrain::default()
            })
            .contains("height_scale_m")
        );
        assert!(
            bad(LevelTerrain {
                height_offset_m: f32::INFINITY,
                ..LevelTerrain::default()
            })
            .contains("height_offset_m")
        );
        let t = LevelTerrain::default();
        assert_eq!(t.map_for("maps/a.level.ron"), "maps/a.level.ron");
        let named = LevelTerrain {
            map: "maps/shared".into(),
            ..LevelTerrain::default()
        };
        assert_eq!(named.map_for("maps/a.level.ron"), "maps/shared");
    }
}
