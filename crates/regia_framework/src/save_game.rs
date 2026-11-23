// SPDX-License-Identifier: MPL-2.0
//! Save games (`docs/specs/14-scene-and-framework.md` § B7, save-games half — settings already
//! exist from M9).
//!
//! [`SaveGame`] writes save files with a header (magic, version, timestamp), postcard-encoded
//! [`SaveFileRaw`] payload, and a CRC-32 footer. Writes are atomic (temp file + rename). An
//! autosave hook runs once on the `Suspended` edge (host and Android).
//!
//! # What gets saved
//!
//! - **Entities** marked with [`Saveable`]: every reflected, serialisable component is encoded
//!   through [`ReflectCodec::to_postcard_typed`] and keyed by its short type path. A component
//!   that is not in the type registry, or that the spec's component filter leaves out, is
//!   skipped with no warning. A postcard encode that fails logs a warning and is skipped. An
//!   entity left with no serialisable component is omitted; load despawns every current
//!   [`Saveable`] entity and spawns only the file's rows, so that entity is gone.
//! - **Resources**: types that implement [`SaveableResource`] and are registered with
//!   [`App::register_saveable_resource`](RegisterSaveableExt::register_saveable_resource) are
//!   collected automatically — games never hand-build the list.
//! - **Sections**: a [`SaveSections`] entry writes its own bytes with the whole world in hand and
//!   reads them back after the load (Lua mod state, ADR-1943). A section is stored as one more
//!   [`SavedResource`] keyed by its key, so the file format does not change.
//!
//! # Loading
//!
//! Loading despawns the current [`Saveable`] entities, spawns the saved ones, and replaces each
//! registered saveable resource. A load removes only what a save records (ADR-7601): a
//! non-`Saveable` child of a `Saveable` entity (an emitter, a camera rig) is detached before its
//! parent goes and re-parented, with its local transform, to the respawned entity of the same
//! stable id (`EntityGuid` and `LevelMember` level, the key of `SaveEntityId::Level`); a child of
//! a runtime parent (no `EntityGuid`) or of a parent the save does not hold stays alive at the
//! root, at the world pose it had. Component payloads missing fields take defaults and unknown
//! fields are skipped, per the engine-wide binary loading rules (`regia_ecs::serialize`).
//!
//! `Entity`-typed *fields inside* components (a pawn's target, a carried item, a seat) are written
//! as stable save ids and remapped on load ([`crate::save_entity_refs`], ADR-3239): such a
//! component type registers `ReflectMapEntities`, or it is not saved (`save_entity_ref_unmapped`).
//!
//! # Format
//!
//! Full spec: `docs/formats/save-format.md`. Briefly:
//! - 4-byte magic `FRTS`
//! - 4-byte LE format version
//! - Postcard-encoded [`SaveFileRaw`] payload (entity + resource data as type-path→bytes maps)
//! - 4-byte LE CRC-32 of the payload
//!
//! # Migration
//!
//! Games register [`SaveMigrations`] with hooks from version N → N+1. Loading a file runs every
//! applicable migration in order; each hook receives a [`MigrationCtx`] with the app's type
//! registry so component payloads can be decoded, changed and re-encoded through
//! [`SaveFileRaw::map_component`].

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::SystemTime;

use regia_app::prelude::*;
use regia_ecs::reflect::TypeRegistry;
use regia_ecs::serialize::ReflectCodec;
use serde::{Deserialize, Serialize};

/// Magic bytes at the start of every save file: `b"FRTS"`.
pub const SAVE_MAGIC: &[u8; 4] = b"FRTS";

/// Current save-file format version. Increment when the on-disk layout changes incompatibly.
/// Version 2 adds [`SaveFileRaw::entity_ids`] (stable entity references, ADR-3239).
pub const SAVE_FORMAT_VERSION: u32 = 2;

/// The largest save file the engine reads (1 GiB). A larger file is refused before any of it is
/// read, so one oversized `.save` in the saves folder cannot exhaust memory while slots are listed.
pub const MAX_SAVE_FILE_BYTES: u64 = 1 << 30;

/// The extension of a slot's previous good generation: `<slot>.prev` beside `<slot>.save`
/// (ADR-3102). A save renames the current file to it before the new one takes its place; a load
/// whose current copy is damaged reads it.
pub const SAVE_PREVIOUS_EXTENSION: &str = "prev";

/// The previous-generation path of a slot file: `<dir>/<slot>.prev` for `<dir>/<slot>.save`.
pub fn previous_path(path: &std::path::Path) -> PathBuf {
    path.with_extension(SAVE_PREVIOUS_EXTENSION)
}

/// Which copy of a slot a load read (ADR-3102).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SaveCopy {
    /// The current file (`<slot>.save`), and every save.
    #[default]
    Current,
    /// The previous good generation (`<slot>.prev`): the current copy was damaged or missing.
    Previous,
}

impl SaveCopy {
    /// Lowercase name for logs and the agent protocol: `current` or `previous`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Previous => "previous",
        }
    }
}

/// The last slot a load read and which copy it used (`save.status` reports it). Inserted by the
/// first load; absent before.
#[derive(Resource, Debug, Clone, PartialEq, Eq)]
pub struct LastSaveLoad {
    /// The slot name.
    pub slot: String,
    /// The copy that loaded.
    pub copy: SaveCopy,
}

/// Marker component: an entity with this component is included in saves.
///
/// Attach to the root entity of a saveable subtree. Only reflected components are serialised;
/// unregistered types are silently skipped.
#[derive(Component, Reflect, Debug, Clone, Default)]
#[reflect(Component, Default)]
pub struct Saveable;

/// Marker trait for resources that participate in save games (§ B7).
///
/// Implement this for a [`Resource`] type, then register it once with
/// [`App::register_saveable_resource`](RegisterSaveableExt::register_saveable_resource) —
/// that call registers the type for reflection and adds it to the list
/// [`SaveGame::save`] collects automatically. The resource itself is (de)serialised through
/// reflection, so no per-type code is needed.
///
/// # Example
///
/// ```
/// use regia_app::prelude::*;
/// use regia_framework::save_game::{RegisterSaveableExt, SaveableResource, SaveableResources};
///
/// #[derive(Resource, Reflect, Default, Clone)]
/// #[reflect(Resource, Default)]
/// struct Score(u32);
///
/// impl SaveableResource for Score {}
///
/// let mut app = App::new();
/// app.add_plugins(MinimalPlugins);
/// app.register_saveable_resource::<Score>();
///
/// let paths: Vec<_> = app.world().resource::<SaveableResources>().type_paths().collect();
/// assert_eq!(paths.len(), 1, "one saveable resource registered: {paths:?}");
/// ```
pub trait SaveableResource: Resource + Reflect + FromReflect + TypePath + GetTypeRegistration {}

/// Reads a registered resource from the world and encodes it; `None` when the resource is
/// absent (nothing to save).
type SerializeFn = fn(&World, &TypeRegistry) -> Option<Result<Vec<u8>, SaveError>>;
/// Decodes `bytes` and replaces the resource in the world.
type RestoreFn = fn(&mut World, &TypeRegistry, &[u8]) -> Result<(), SaveError>;
/// Decodes `bytes` without touching the world: a load checks every payload before it changes
/// anything.
type CheckFn = fn(&TypeRegistry, &[u8]) -> Result<(), SaveError>;

/// Decodes a saved resource payload as `T`.
fn decode_resource<T: SaveableResource>(registry: &TypeRegistry, bytes: &[u8]) -> Result<T, SaveError> {
    let Some(registration) = registry.get(std::any::TypeId::of::<T>()) else {
        return Err(SaveError::Deserialize(format!(
            "resource type {} is not registered for reflection",
            T::type_path()
        )));
    };
    let mut codec = ReflectCodec::new(registry);
    let value = codec
        .from_postcard_typed(bytes, registration)
        .map_err(|e| SaveError::Deserialize(format!("resource {}: {e}", T::type_path())))?;
    <T as FromReflect>::from_reflect(value.as_partial_reflect()).ok_or_else(|| {
        SaveError::Deserialize(format!(
            "payload for resource {} has the wrong type",
            T::type_path()
        ))
    })
}

/// One auto-collected saveable resource type: its full type path plus the type-erased
/// save/restore functions generated by [`SaveableResources::register`].
#[derive(Debug)]
struct SaveableEntry {
    /// Full reflected type path (`T::type_path()`), the key in [`SavedResource::type_path`].
    type_path: &'static str,
    /// Reads the resource from the world and encodes it.
    serialize: SerializeFn,
    /// Decodes `bytes` and replaces the resource in the world.
    restore: RestoreFn,
    /// Decodes `bytes` only (the load's check before it changes the world).
    check: CheckFn,
}

/// The auto-collected list of saveable resource types (§ B7).
///
/// Games do not build this by hand: implement [`SaveableResource`] and call
/// [`App::register_saveable_resource`](RegisterSaveableExt::register_saveable_resource).
#[derive(Resource, Debug, Default)]
pub struct SaveableResources {
    entries: Vec<SaveableEntry>,
}

impl SaveableResources {
    /// Registers `T` as a saveable resource (idempotent: the second call is a no-op).
    pub fn register<T: SaveableResource>(&mut self) {
        if self.entries.iter().any(|entry| entry.type_path == T::type_path()) {
            return;
        }
        self.entries.push(SaveableEntry {
            type_path: T::type_path(),
            serialize: |world, registry| {
                let resource = world.get_resource::<T>()?;
                let mut codec = ReflectCodec::new(registry);
                Some(
                    codec
                        .to_postcard_typed(resource.as_partial_reflect())
                        .map_err(|e| SaveError::Serialize(e.to_string())),
                )
            },
            restore: |world, registry, bytes| {
                let typed = decode_resource::<T>(registry, bytes)?;
                world.insert_resource(typed);
                Ok(())
            },
            check: |registry, bytes| decode_resource::<T>(registry, bytes).map(drop),
        });
    }

    /// The registered resource type paths, in registration order (for inspection and tests).
    pub fn type_paths(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|entry| entry.type_path)
    }
}

/// [`App`] extension: [`App::register_saveable_resource`] (§ B7 auto-collection).
pub trait RegisterSaveableExt {
    /// Marks `T` as a saveable resource: registers the type for reflection and adds it to the
    /// list [`SaveGame::save`] collects automatically. `T` must implement [`SaveableResource`].
    ///
    /// See [`SaveableResource`] for a complete, runnable example.
    fn register_saveable_resource<T: SaveableResource>(&mut self) -> &mut Self;
}

impl RegisterSaveableExt for App {
    fn register_saveable_resource<T: SaveableResource>(&mut self) -> &mut Self {
        self.register_type::<T>()
            .init_resource::<SaveableResources>()
            .world_mut()
            .resource_mut::<SaveableResources>()
            .register::<T>();
        self
    }
}

/// Writes a section's bytes; runs before the entities are serialized, so it may still change the
/// world. `Ok(None)` leaves the section out of this save.
pub type SaveSectionWrite = fn(&mut World) -> Result<Option<Vec<u8>>, SaveError>;
/// Reads a section back; runs after the entities and resources are restored. The bytes are `None`
/// when the save has no such section (a save written before the section existed).
pub type SaveSectionRead = fn(&mut World, Option<&[u8]>) -> Result<(), SaveError>;

/// One registered section.
#[derive(Debug, Clone, Copy)]
struct SaveSection {
    key: &'static str,
    write: SaveSectionWrite,
    read: SaveSectionRead,
    required: bool,
}

/// Save data a system produces itself: state that is not a reflected component or resource and
/// needs `&mut World` to collect or to apply (ADR-1943). Every path through [`SaveGame`] runs the
/// sections: save, encode, load, decode, the save flow and the autosave.
///
/// A section is stored as a [`SavedResource`] with the key as its type path, and a
/// [`SaveSpec::resource_filter`] selects it like a resource. A failing section warns and the rest
/// of the save or load carries on. A section registered with [`Self::register_required`] instead
/// aborts the save when its write fails, before any storage is replaced; read errors still warn.
#[derive(Resource, Debug, Clone, Default)]
pub struct SaveSections {
    sections: Vec<SaveSection>,
}

impl SaveSections {
    /// Registers section `key` (idempotent: the second call for a key is a no-op).
    pub fn register(&mut self, key: &'static str, write: SaveSectionWrite, read: SaveSectionRead) {
        self.register_with_policy(key, write, read, false);
    }

    /// Registers a section whose write error aborts the save before any storage is replaced.
    ///
    /// Use for state that must not be silently omitted. `Ok(None)` still omits the section,
    /// resource filters still select it, and read errors still warn. Registration is idempotent:
    /// a second registration of the same key keeps the original callbacks and policy.
    pub fn register_required(&mut self, key: &'static str, write: SaveSectionWrite, read: SaveSectionRead) {
        self.register_with_policy(key, write, read, true);
    }

    fn register_with_policy(
        &mut self,
        key: &'static str,
        write: SaveSectionWrite,
        read: SaveSectionRead,
        required: bool,
    ) {
        if self.sections.iter().any(|section| section.key == key) {
            return;
        }
        self.sections.push(SaveSection {
            key,
            write,
            read,
            required,
        });
    }

    /// The registered keys, in registration order.
    pub fn keys(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.sections.iter().map(|section| section.key)
    }
}

/// Which entities and resources to save.
#[derive(Debug, Clone, Default)]
pub struct SaveSpec {
    /// Component type short paths to include; empty = all registered components.
    pub component_filter: Vec<String>,
    /// Resource type paths to include; empty = all registered saveable resources.
    pub resource_filter: Vec<String>,
}

// ---------------------------------------------------------------------------
// On-disk format types
// ---------------------------------------------------------------------------

/// Per-entity data in a save file: component type short paths → postcard bytes.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SavedEntity {
    /// Component type short path → postcard bytes (typed payloads, `ReflectCodec`-encoded).
    pub components: BTreeMap<String, Vec<u8>>,
}

/// Per-resource data in a save file.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SavedResource {
    /// Full resource type path.
    pub type_path: String,
    /// Postcard bytes (typed payload, `ReflectCodec`-encoded).
    pub data: Vec<u8>,
}

/// Raw on-disk save file, the target of migration hooks.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[non_exhaustive]
pub struct SaveFileRaw {
    /// Format version (what this file was written with).
    pub version: u32,
    /// Game version string.
    pub game_version: String,
    /// Regia engine version.
    pub engine_version: String,
    /// When the save was created (seconds since UNIX epoch).
    pub timestamp_secs: f64,
    /// Optional screenshot thumbnail (PNG bytes). Defaults to `None`.
    ///
    /// Note: postcard requires every struct field to be serialised (positional format); we always
    /// emit this field so deserialisation can find the succeeding fields.
    #[serde(default)]
    pub thumbnail: Option<Vec<u8>>,
    /// Entity data, one entry per saved entity.
    pub entities: Vec<SavedEntity>,
    /// Resource data.
    pub resources: Vec<SavedResource>,
    /// The save id table (format 2, ADR-3239): one entry per saved entity in [`Self::entities`]
    /// order, then the level entities saved components point at. Empty in a file whose entity
    /// references were never converted (format 1); the loader converts them.
    #[serde(default)]
    pub entity_ids: Vec<crate::save_entity_refs::SaveEntityId>,
}

/// The serde field-list fingerprint of the save-file payload (hashed by `cargo xtask check-formats`).
///
/// The root struct *and* its nested entity/resource structs are listed.
pub fn save_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<SaveFileRaw>(),
        regia_core::schema::serde_field_names::<SavedEntity>(),
        regia_core::schema::serde_field_names::<SavedResource>(),
    ])
}

/// The built-in migration sources reported to `cargo xtask check-formats`: the engine converts
/// format 1 itself (entity references, ADR-3239). Game data migrations are registered by the game
/// at runtime ([`SaveMigrations`]).
pub fn save_migration_sources() -> Vec<u32> {
    vec![1]
}

impl SaveFileRaw {
    /// Decodes every saved component stored under `short_path`, hands the reflected value to
    /// `visit`, and re-encodes it in place. Returns how many entities were rewritten.
    ///
    /// Migration hooks use this to change component data across versions — rescale values,
    /// move data between fields, rename keys in the map. The value is decoded with the
    /// **current** type's reflection: fields whose *types* changed since the file was written
    /// must be rewritten by hand (the postcard layout is positional), or the old type kept
    /// registered and mapped field-by-field.
    pub fn map_component(
        &mut self,
        registry: &TypeRegistry,
        short_path: &str,
        visit: &mut dyn FnMut(&mut dyn PartialReflect) -> Result<(), SaveError>,
    ) -> Result<usize, SaveError> {
        let mut codec = ReflectCodec::new(registry);
        let mut touched = 0;
        for entity in &mut self.entities {
            let Some(bytes) = entity.components.get_mut(short_path) else {
                continue;
            };
            let Some(registration) = registry.get_with_short_type_path(short_path) else {
                return Err(SaveError::Deserialize(format!(
                    "component type {short_path:?} is not registered for reflection"
                )));
            };
            let value = codec
                .from_postcard_typed(bytes, registration)
                .map_err(|e| SaveError::Deserialize(e.to_string()))?;
            let mut value: Box<dyn PartialReflect> = value;
            visit(value.as_partial_reflect_mut())?;
            *bytes = codec
                .to_postcard_typed(value.as_partial_reflect())
                .map_err(|e| SaveError::Serialize(e.to_string()))?;
            touched += 1;
        }
        Ok(touched)
    }
}

// ---------------------------------------------------------------------------
// Save / load
// ---------------------------------------------------------------------------

/// A save slot: an identified save file on disk.
#[derive(Debug, Clone)]
pub struct SaveSlot {
    /// The slot name (directory-safe, e.g. "Auto", "Slot1", "Quick").
    pub name: String,
    /// The on-disk path.
    pub path: PathBuf,
    /// The save header (loaded metadata, `None` if the file doesn't exist).
    pub header: Option<SaveHeader>,
}

impl SaveSlot {
    /// The slot's previous-generation file, `<dir>/<slot>.prev` (ADR-3102).
    pub fn previous_path(&self) -> PathBuf {
        previous_path(&self.path)
    }

    /// Whether the slot has a previous generation on disk.
    pub fn has_previous(&self) -> bool {
        self.previous_path().is_file()
    }
}

/// Metadata read quickly without deserialising the full save.
#[derive(Debug, Clone)]
pub struct SaveHeader {
    /// Format version.
    pub version: u32,
    /// Game version.
    pub game_version: String,
    /// Engine version.
    pub engine_version: String,
    /// Creation timestamp.
    pub timestamp_secs: f64,
    /// Whether a thumbnail is present.
    pub has_thumbnail: bool,
}

/// The save/load subsystem.
pub struct SaveGame;

impl SaveGame {
    /// Serialises [`Saveable`] entities and registered saveable resources, and writes `slot`
    /// atomically. Returns how many entities/resources were written and the file size in bytes.
    ///
    /// The slot keeps one previous good generation: a current file that passes its check is
    /// renamed to `<slot>.prev` first (ADR-3102). A failed write leaves both files as they were.
    pub fn save(slot: &SaveSlot, world: &mut World, spec: &SaveSpec) -> Result<SaveResult, SaveError> {
        let raw = Self::encode_raw(world, spec)?;
        let bytes_written = write_raw(slot, &raw)?;

        let entity_count = raw.entities.len();
        let resource_count = raw.resources.len();
        tracing::info!(
            target: "regia_framework",
            "saved {} entities and {} resources to slot {}",
            entity_count, resource_count, slot.name
        );

        Ok(SaveResult {
            entity_count,
            resource_count,
            bytes_written,
            copy: SaveCopy::Current,
        })
    }

    /// Serialises the world into save-file **bytes** — the exact container
    /// `SaveGame::save` writes to disk (magic, version, postcard payload, CRC footer).
    ///
    /// The backend seam for non-file storage (§ B7): a cloud backend such as
    /// `regia_steam`'s `SteamRemoteStorage` stores these bytes, and
    /// [`SaveGame::decode`] restores them into a world — no filesystem involved.
    pub fn encode(world: &mut World, spec: &SaveSpec) -> Result<Vec<u8>, SaveError> {
        Self::encode_raw(world, spec).and_then(|raw| frame_save_file(&raw))
    }

    /// [`SaveGame::encode`] plus the counts [`SaveGame::save`] reports (`bytes_written` is the
    /// container length). Used by the save flow's storages (`crate::save_flow`).
    pub fn encode_counted(world: &mut World, spec: &SaveSpec) -> Result<(Vec<u8>, SaveResult), SaveError> {
        let raw = Self::encode_raw(world, spec)?;
        let bytes = frame_save_file(&raw)?;
        let result = SaveResult {
            entity_count: raw.entities.len(),
            resource_count: raw.resources.len(),
            bytes_written: bytes.len(),
            copy: SaveCopy::Current,
        };
        Ok((bytes, result))
    }

    /// Validates, migrates and applies save-file **bytes** (from [`SaveGame::encode`]) to
    /// `world`: existing saveable entities are despawned, saved entities spawn back, and
    /// registered resources are replaced. The byte-level twin of [`SaveGame::load`].
    pub fn decode(world: &mut World, bytes: &[u8], spec: &SaveSpec) -> Result<SaveResult, SaveError> {
        let (raw, bytes_read) = parse_save_file(bytes, world)?;
        Self::apply(world, &raw, spec)?;
        Ok(SaveResult {
            entity_count: raw.entities.len(),
            resource_count: raw.resources.len(),
            bytes_written: bytes_read,
            copy: SaveCopy::Current,
        })
    }

    /// Saves a pre-built [`SaveFileRaw`] directly, skipping world serialisation.
    ///
    /// Useful for tests, migration tools, or games that build their own save data.
    pub fn save_raw(slot: &SaveSlot, raw: &SaveFileRaw) -> Result<SaveResult, SaveError> {
        let bytes_written = write_raw(slot, raw)?;
        Ok(SaveResult {
            entity_count: raw.entities.len(),
            resource_count: raw.resources.len(),
            bytes_written,
            copy: SaveCopy::Current,
        })
    }

    /// Loads and deserialises `slot`, applying registered migrations, then spawns entities and
    /// replaces resources in `world`. Existing saveable entities are despawned first.
    ///
    /// A current file that is damaged (`SaveError::Corrupt`), or missing while `<slot>.prev`
    /// exists, loads the previous generation instead with one warn line; [`SaveResult::copy`]
    /// says which copy loaded (ADR-3102).
    pub fn load(slot: &SaveSlot, world: &mut World, spec: &SaveSpec) -> Result<SaveResult, SaveError> {
        let (raw, bytes_read, copy) = read_raw_full(slot, world)?;
        Self::apply(world, &raw, spec)?;
        world.insert_resource(LastSaveLoad {
            slot: slot.name.clone(),
            copy,
        });

        tracing::info!(
            target: "regia_framework",
            "loaded {} entities and {} resources from slot {} ({} copy)",
            raw.entities.len(), raw.resources.len(), slot.name, copy.as_str()
        );

        Ok(SaveResult {
            entity_count: raw.entities.len(),
            resource_count: raw.resources.len(),
            bytes_written: bytes_read,
            copy,
        })
    }

    /// Deletes `slot` on purpose: the current file and its previous generation. `Ok(false)` when
    /// neither existed.
    ///
    /// # Errors
    /// An IO failure other than "not found".
    pub fn delete(slot: &SaveSlot) -> Result<bool, SaveError> {
        delete_slot_files(&slot.path)
    }

    /// Applies an already parsed and migrated payload (the save flow's load, after its fallback
    /// choice) and builds the result.
    pub(crate) fn apply_parsed(
        world: &mut World,
        raw: &SaveFileRaw,
        bytes_read: usize,
        copy: SaveCopy,
        spec: &SaveSpec,
    ) -> Result<SaveResult, SaveError> {
        Self::apply(world, raw, spec)?;
        Ok(SaveResult {
            entity_count: raw.entities.len(),
            resource_count: raw.resources.len(),
            bytes_written: bytes_read,
            copy,
        })
    }

    /// The shared body of [`SaveGame::load`] and [`SaveGame::decode`].
    fn apply(world: &mut World, raw: &SaveFileRaw, spec: &SaveSpec) -> Result<(), SaveError> {
        let registry = world
            .get_resource::<AppTypeRegistry>()
            .cloned()
            .unwrap_or_default();
        let registry = registry.read();
        deserialize_world(world, raw, &registry, spec)
    }

    /// Serialises the world into a [`SaveFileRaw`] (no container framing).
    fn encode_raw(world: &mut World, spec: &SaveSpec) -> Result<SaveFileRaw, SaveError> {
        let registry = world
            .get_resource::<AppTypeRegistry>()
            .cloned()
            .unwrap_or_default();
        let registry = registry.read();
        serialize_world(world, &registry, spec)
    }

    /// Loads and returns the raw save file (migrations applied), without deserialising into the
    /// world.
    pub fn load_raw(slot: &SaveSlot, world: &mut World) -> Result<SaveFileRaw, SaveError> {
        read_raw_full(slot, world).map(|(raw, _, _)| raw)
    }

    /// Lists all save slots in the [`SaveDirectory`], with headers read from each file.
    pub fn list_slots(dir: &SaveDirectory) -> Vec<SaveSlot> {
        let mut slots = Vec::new();
        let Ok(entries) = std::fs::read_dir(&dir.0) else {
            return slots;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "save") {
                let name = path.file_stem().unwrap_or_default().to_string_lossy().to_string();
                let header = read_header(&path).or_else(|| read_header(&previous_path(&path)));
                slots.push(SaveSlot { name, path, header });
            } else if path.extension().is_some_and(|ext| ext == SAVE_PREVIOUS_EXTENSION) {
                // A slot whose current file is gone (a crash between the two renames of a save)
                // is still listed; its load reads the previous copy.
                let current = path.with_extension("save");
                if !current.exists() {
                    let name = path.file_stem().unwrap_or_default().to_string_lossy().to_string();
                    let header = read_header(&path);
                    slots.push(SaveSlot {
                        name,
                        path: current,
                        header,
                    });
                }
            }
        }
        slots.sort_by(|a, b| a.name.cmp(&b.name));
        slots
    }

    /// Creates a [`SaveSlot`] for `name` in `dir`. Does not write anything. The name goes through
    /// [`sanitize_slot_name`](crate::save_flow::sanitize_slot_name); an empty name becomes `_`, so
    /// the file has a stem [`SaveGame::list_slots`] can list.
    pub fn slot(dir: &SaveDirectory, name: &str) -> SaveSlot {
        let mut safe_name = crate::save_flow::sanitize_slot_name(name);
        if safe_name.is_empty() {
            safe_name.push('_');
        }
        let path = dir.0.join(format!("{safe_name}.save"));
        let header = read_header(&path).or_else(|| read_header(&previous_path(&path)));
        SaveSlot {
            name: safe_name,
            path,
            header,
        }
    }
}

/// The result of a save or load operation.
#[derive(Debug, Clone)]
pub struct SaveResult {
    /// How many entities were saved/loaded.
    pub entity_count: usize,
    /// How many resources were saved/loaded.
    pub resource_count: usize,
    /// Bytes written (save) or read (load).
    pub bytes_written: usize,
    /// Which copy a load read: [`SaveCopy::Previous`] when the current file was damaged or
    /// missing and `<slot>.prev` loaded (ADR-3102). Always [`SaveCopy::Current`] for a save.
    pub copy: SaveCopy,
}

// ---------------------------------------------------------------------------
// Directory
// ---------------------------------------------------------------------------

/// Where save files live (default `user://saves/`).
#[derive(Resource, Debug, Clone)]
pub struct SaveDirectory(pub PathBuf);

impl Default for SaveDirectory {
    fn default() -> Self {
        // On Android too: a relative `saves` resolves against the process's read-only `/`, so
        // every autosave on suspend failed with `Read-only file system` (found on device).
        Self(default_save_dir(regia_platform::paths::user_dir()))
    }
}

/// `<user dir>/saves`, or `./saves` when the platform reports no user directory.
fn default_save_dir(user: Option<PathBuf>) -> PathBuf {
    user.unwrap_or_else(|| PathBuf::from(".")).join("saves")
}

// ---------------------------------------------------------------------------
// Migrations
// ---------------------------------------------------------------------------

/// What a migration hook receives: the version it migrates *from* and the running app's type
/// registry (so component payloads can be decoded, changed and re-encoded through
/// [`SaveFileRaw::map_component`]).
pub struct MigrationCtx<'a> {
    /// The format version this step migrates from (the file's on-disk version).
    pub from_version: u32,
    /// The running app's reflection registry.
    pub registry: &'a TypeRegistry,
}

/// A migration function: transforms a save file from `ctx.from_version` to
/// `ctx.from_version + 1`.
pub type MigrationFn = fn(&MigrationCtx<'_>, &mut SaveFileRaw);

/// Registered migration functions, keyed by the version they migrate *from*.
#[derive(Resource, Debug, Clone, Default)]
pub struct SaveMigrations {
    functions: BTreeMap<u32, MigrationFn>,
}

impl SaveMigrations {
    /// Registers a migration from `version` to `version + 1`.
    pub fn register(&mut self, from_version: u32, migrate: MigrationFn) {
        self.functions.insert(from_version, migrate);
    }

    /// The migration for `version`, if registered.
    pub fn get(&self, version: u32) -> Option<&MigrationFn> {
        self.functions.get(&version)
    }
}

// ---------------------------------------------------------------------------
// Core I/O
// ---------------------------------------------------------------------------

/// Frames `raw` into save-file bytes: magic, format version, postcard payload, CRC footer.
///
/// The header carries `raw.version`, the version the payload is in: a migration tool that writes
/// an old payload through [`SaveGame::save_raw`] gets a file whose migrations run on load (the
/// world path always writes [`SAVE_FORMAT_VERSION`]). A payload newer than this engine is refused,
/// and so is a format 1 payload with entity ids: format 1 has no id table, so writing it would
/// drop every stable entity reference without a word.
fn frame_save_file(raw: &SaveFileRaw) -> Result<Vec<u8>, SaveError> {
    if raw.version > SAVE_FORMAT_VERSION {
        return Err(SaveError::Serialize(format!(
            "payload version {} is newer than this engine writes ({SAVE_FORMAT_VERSION})",
            raw.version
        )));
    }
    let since = crate::save_entity_refs::ENTITY_IDS_SINCE;
    if raw.version < since && !raw.entity_ids.is_empty() {
        return Err(SaveError::Serialize(format!(
            "payload version {} has no entity id table but {} ids were given; write version {since} \
             or later, or clear entity_ids",
            raw.version,
            raw.entity_ids.len()
        )));
    }
    let mut bytes = Vec::new();
    bytes.extend_from_slice(SAVE_MAGIC);
    bytes.extend_from_slice(&raw.version.to_le_bytes());
    let payload =
        crate::save_entity_refs::encode_payload(raw).map_err(|e| SaveError::Serialize(e.to_string()))?;
    bytes.extend_from_slice(&payload);
    bytes.extend_from_slice(&crc32(&payload).to_le_bytes());
    Ok(bytes)
}

/// Writes `raw` to `slot` with magic, version, postcard payload, and CRC footer. Returns the
/// number of bytes written.
fn write_raw(slot: &SaveSlot, raw: &SaveFileRaw) -> Result<usize, SaveError> {
    let bytes = frame_save_file(raw)?;
    write_atomic_keep_previous(&slot.path, &bytes)?;
    Ok(bytes.len())
}

/// Parses, validates and migrates save-file `bytes`. Returns the raw payload and the input
/// length. Used by both the file path ([`SaveGame::load`]) and the byte path
/// ([`SaveGame::decode`]).
pub(crate) fn parse_save_file(bytes: &[u8], world: &mut World) -> Result<(SaveFileRaw, usize), SaveError> {
    let len = bytes.len();
    if bytes.len() < 12 {
        return Err(SaveError::Corrupt("file too short".into()));
    }
    if &bytes[..4] != SAVE_MAGIC {
        return Err(SaveError::Corrupt("bad magic".into()));
    }
    let file_version = u32::from_le_bytes(bytes[4..8].try_into().expect("4-byte slice is infallible")); // invariant: len >= 12 checked above
    let crc_stored = u32::from_le_bytes(
        bytes[bytes.len() - 4..]
            .try_into()
            .expect("4-byte tail slice is infallible"), // invariant: len >= 12 checked above
    );
    let payload = &bytes[8..bytes.len() - 4];

    // The CRC first: damage anywhere in the payload is `Corrupt` (and a load falls back to the
    // previous copy, ADR-3102) even when it also garbles the header's version.
    let crc_computed = crc32(payload);
    if crc_computed != crc_stored {
        return Err(SaveError::Corrupt(format!(
            "CRC mismatch: stored {crc_stored:08x}, computed {crc_computed:08x}"
        )));
    }
    // Reject a save written by a newer engine before decoding its (unknown-shaped) payload. The
    // header's version is outside the CRC; the payload repeats it as its first field. A header
    // that claims a newer version over a payload of a version this engine knows was damaged.
    if file_version > SAVE_FORMAT_VERSION {
        if let Some(payload_version) = leading_varint_u32(payload)
            && payload_version <= SAVE_FORMAT_VERSION
        {
            return Err(SaveError::Corrupt(format!(
                "header version {file_version} does not match the payload's version {payload_version}"
            )));
        }
        return Err(SaveError::Deserialize(format!(
            "save file version {file_version} is newer than this engine supports ({SAVE_FORMAT_VERSION})"
        )));
    }

    // The payload layout follows the CRC-covered payload version (its first field), not the
    // header word, for the same reason the migrations below do.
    // A payload that does not begin with a decodable version varint can never load (the postcard
    // decode of the same bytes fails first); refuse it by name, as `upgrade_save_bytes` does,
    // instead of reading through the header word, which the CRC does not cover.
    let layout_version = leading_varint_u32(payload).ok_or_else(|| {
        SaveError::Deserialize("payload does not begin with a decodable version varint".into())
    })?;
    let mut raw: SaveFileRaw = crate::save_entity_refs::decode_payload(layout_version, payload)
        .map_err(|e| SaveError::Deserialize(e.to_string()))?;

    // Migrations follow the CRC-covered payload version. The header word is outside the CRC,
    // so a cleared header over an intact current payload is damage to the header, not a
    // version-0 file that needs a migration.
    if raw.version > SAVE_FORMAT_VERSION {
        return Err(SaveError::Deserialize(format!(
            "save file version {} is newer than this engine supports ({SAVE_FORMAT_VERSION})",
            raw.version
        )));
    }

    // Run migrations from the payload's version up to the current one, in order.
    if raw.version < SAVE_FORMAT_VERSION {
        let migrations = world
            .get_resource::<SaveMigrations>()
            .cloned()
            .unwrap_or_default();
        let registry = world
            .get_resource::<AppTypeRegistry>()
            .cloned()
            .unwrap_or_default();
        let registry = registry.read();
        // Every step must exist: applying a file with one step skipped as if it were current
        // would load wrong data without a word (audit v034 net M2).
        if let Some(missing) = (raw.version..SAVE_FORMAT_VERSION)
            .find(|v| migrations.get(*v).is_none() && !crate::save_entity_refs::engine_step(*v))
        {
            return Err(SaveError::Deserialize(format!(
                "save file version {} needs a migration from version {missing} to {}, and none is registered",
                raw.version,
                missing + 1
            )));
        }
        for version in raw.version..SAVE_FORMAT_VERSION {
            if let Some(migrate) = migrations.get(version) {
                tracing::info!(
                    target: "regia_framework",
                    "migrating save from version {version} to {}",
                    version + 1
                );
                migrate(
                    &MigrationCtx {
                        from_version: version,
                        registry: &registry,
                    },
                    &mut raw,
                );
                raw.version = version + 1;
            }
        }
    }
    // References of a file written before format 2 become `Lost` (ADR-3239).
    if let Some(registry) = world.get_resource::<AppTypeRegistry>().cloned() {
        crate::save_entity_refs::convert_legacy_refs(&mut raw, &registry.read());
    }

    Ok((raw, len))
}

/// The postcard varint (LEB128) `u32` at the start of `bytes`: a save payload's `version` field.
pub(crate) fn leading_varint_u32(bytes: &[u8]) -> Option<u32> {
    let mut value: u32 = 0;
    for (i, &byte) in bytes.iter().take(5).enumerate() {
        let part = u32::from(byte & 0x7f);
        value |= part.checked_shl(7 * i as u32)?;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

/// Reads, validates and migrates a save file, falling back to its previous generation when the
/// current copy is damaged or missing (ADR-3102). Returns the raw payload, the size in bytes and
/// the copy that loaded.
fn read_raw_full(slot: &SaveSlot, world: &mut World) -> Result<(SaveFileRaw, usize, SaveCopy), SaveError> {
    let previous = previous_path(&slot.path);
    let current = read_save_bytes_optional(&slot.path);
    match parse_with_fallback(
        world,
        &slot.name,
        current,
        &slot.path.display(),
        &mut || read_save_bytes_optional(&previous),
        &previous.display(),
    )? {
        Some(found) => Ok(found),
        None => Err(SaveError::Io(
            std::io::Error::new(std::io::ErrorKind::NotFound, "no such save slot"),
            slot.path.clone(),
        )),
    }
}

/// [`read_save_bytes`] with "not found" as `Ok(None)`.
fn read_save_bytes_optional(path: &std::path::Path) -> Result<Option<Vec<u8>>, SaveError> {
    match read_save_bytes(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(SaveError::Io(e, _)) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Parses a slot's current copy, and its previous generation when the current one is damaged
/// (`SaveError::Corrupt`) or missing (ADR-3102). `Ok(None)`: neither copy exists. A fallback
/// logs one warn line naming the slot, both copies and the reason.
///
/// Other failures of the current copy (an IO error, a newer format version, a missing migration)
/// are returned as they are: the previous copy would hide them or roll the player back.
pub(crate) fn parse_with_fallback(
    world: &mut World,
    slot: &str,
    current: Result<Option<Vec<u8>>, SaveError>,
    current_label: &dyn std::fmt::Display,
    previous: &mut dyn FnMut() -> Result<Option<Vec<u8>>, SaveError>,
    previous_label: &dyn std::fmt::Display,
) -> Result<Option<(SaveFileRaw, usize, SaveCopy)>, SaveError> {
    let damage = match current {
        Ok(Some(bytes)) => match parse_save_file(&bytes, world) {
            Ok((raw, len)) => return Ok(Some((raw, len, SaveCopy::Current))),
            Err(SaveError::Corrupt(why)) => Some(why),
            Err(e) => return Err(e),
        },
        Ok(None) => None,
        Err(SaveError::Corrupt(why)) => Some(why),
        Err(e) => return Err(e),
    };
    let previous_bytes = match previous() {
        Ok(found) => found,
        // The previous copy cannot be read: report the current copy's damage, which is what the
        // player asked for.
        Err(e) => {
            return match damage {
                Some(why) => Err(SaveError::Corrupt(format!(
                    "{why}; the previous copy {previous_label} cannot be read either: {e}"
                ))),
                None => Err(e),
            };
        }
    };
    let Some(previous_bytes) = previous_bytes else {
        return match damage {
            Some(why) => Err(SaveError::Corrupt(why)),
            None => Ok(None),
        };
    };
    match parse_save_file(&previous_bytes, world) {
        Ok((raw, len)) => {
            let what = match &damage {
                Some(why) => format!("is damaged ({why})"),
                None => "is missing".to_owned(),
            };
            tracing::warn!(
                target: "regia_framework",
                "save slot {slot}: the current copy {current_label} {what}; loaded the previous copy {previous_label}"
            );
            Ok(Some((raw, len, SaveCopy::Previous)))
        }
        Err(e) => match damage {
            Some(why) => Err(SaveError::Corrupt(format!(
                "{why}; the previous copy {previous_label} is damaged too: {e}"
            ))),
            None => Err(e),
        },
    }
}

/// Whether save-file `bytes` pass the container check: length, magic and CRC-32. A save rotates
/// the current file to `<slot>.prev` only when it passes, so damage never replaces a good
/// previous copy.
pub(crate) fn frame_is_intact(bytes: &[u8]) -> bool {
    if bytes.len() < 12 || &bytes[..4] != SAVE_MAGIC {
        return false;
    }
    let Ok(tail) = <[u8; 4]>::try_from(&bytes[bytes.len() - 4..]) else {
        return false;
    };
    crc32(&bytes[8..bytes.len() - 4]) == u32::from_le_bytes(tail)
}

/// A save file's bytes, refusing one past [`MAX_SAVE_FILE_BYTES`] before reading it (and reading
/// at most one byte past the cap, in case it grew after the size check).
pub(crate) fn read_save_bytes(path: &std::path::Path) -> Result<Vec<u8>, SaveError> {
    use std::io::Read as _;
    let too_large = || {
        SaveError::Corrupt(format!(
            "{} is larger than the {MAX_SAVE_FILE_BYTES} bytes a save file may be",
            path.display()
        ))
    };
    let file = std::fs::File::open(path).map_err(|e| SaveError::Io(e, path.to_path_buf()))?;
    let len = file
        .metadata()
        .map_err(|e| SaveError::Io(e, path.to_path_buf()))?
        .len();
    if len > MAX_SAVE_FILE_BYTES {
        return Err(too_large());
    }
    let mut bytes = Vec::with_capacity(usize::try_from(len).unwrap_or(0));
    file.take(MAX_SAVE_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| SaveError::Io(e, path.to_path_buf()))?;
    if bytes.len() as u64 > MAX_SAVE_FILE_BYTES {
        return Err(too_large());
    }
    Ok(bytes)
}

// ---------------------------------------------------------------------------
// Serialisation
// ---------------------------------------------------------------------------

/// Collects saveable entities and registered saveable resources from the world.
fn serialize_world(
    world: &mut World,
    registry: &TypeRegistry,
    spec: &SaveSpec,
) -> Result<SaveFileRaw, SaveError> {
    // Sections first: one may still change the world the entities below are read from.
    let sections = write_sections(world, spec)?;

    // Entities in save-id order, `Entity` fields as save ids (ADR-3239).
    let crate::save_entity_refs::SavedEntities { entities, entity_ids } =
        crate::save_entity_refs::serialize_entities(world, registry, spec);

    // Resources: every registered saveable type present in the world, filtered by the spec.
    let mut resources: Vec<SavedResource> = Vec::new();
    if let Some(collected) = world.get_resource::<SaveableResources>() {
        for entry in &collected.entries {
            if !spec.resource_filter.is_empty()
                && !spec
                    .resource_filter
                    .iter()
                    .any(|f| entry.type_path.contains(f.as_str()))
            {
                continue;
            }
            match (entry.serialize)(world, registry) {
                None => {} // resource not inserted yet: skip
                Some(Ok(data)) => resources.push(SavedResource {
                    type_path: entry.type_path.to_string(),
                    data,
                }),
                Some(Err(e)) => {
                    tracing::warn!(
                        target: "regia_framework",
                        "cannot serialize resource {}: {e}",
                        entry.type_path
                    );
                }
            }
        }
    }

    resources.extend(sections);

    let timestamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();

    Ok(SaveFileRaw {
        version: SAVE_FORMAT_VERSION,
        game_version: option_env!("CARGO_PKG_VERSION").unwrap_or("unknown").into(),
        engine_version: env!("CARGO_PKG_VERSION").into(),
        timestamp_secs: timestamp,
        thumbnail: None,
        entities,
        resources,
        entity_ids,
    })
}

/// Runs every section's write, filtered like a resource.
fn write_sections(world: &mut World, spec: &SaveSpec) -> Result<Vec<SavedResource>, SaveError> {
    let Some(sections) = world.get_resource::<SaveSections>().cloned() else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for section in &sections.sections {
        if !spec.resource_filter.is_empty()
            && !spec
                .resource_filter
                .iter()
                .any(|f| section.key.contains(f.as_str()))
        {
            continue;
        }
        match (section.write)(world) {
            Ok(Some(data)) => out.push(SavedResource {
                type_path: section.key.to_string(),
                data,
            }),
            Ok(None) => {}
            Err(e) if section.required => return Err(e),
            Err(e) => tracing::warn!(target: "regia_framework", "cannot save section {}: {e}", section.key),
        }
    }
    Ok(out)
}

/// Runs every section's read, with its bytes from `raw` (`None` when the save lacks it).
fn read_sections(world: &mut World, raw: &SaveFileRaw) {
    let Some(sections) = world.get_resource::<SaveSections>().cloned() else {
        return;
    };
    for section in &sections.sections {
        let data = raw
            .resources
            .iter()
            .find(|saved| saved.type_path == section.key)
            .map(|saved| saved.data.as_slice());
        if let Err(e) = (section.read)(world, data) {
            tracing::warn!(target: "regia_framework", "cannot load section {}: {e}", section.key);
        }
    }
}

/// Deserialises entities and resources from `raw` into `world`.
///
/// Every payload is decoded before the world changes: one that does not decode fails the load
/// and leaves the live world as it was (audit v034 net M1). A component or resource type the app
/// does not register is skipped with a warning (a type the game removed), as before.
fn deserialize_world(
    world: &mut World,
    raw: &SaveFileRaw,
    registry: &TypeRegistry,
    _spec: &SaveSpec,
) -> Result<(), SaveError> {
    let mut codec = ReflectCodec::new(registry);

    // Decode every component first.
    let mut staged = Vec::with_capacity(raw.entities.len());
    for saved in &raw.entities {
        let mut components = Vec::with_capacity(saved.components.len());
        for (short_path, bytes) in &saved.components {
            let Some(reg) = registry.get_with_short_type_path(short_path) else {
                tracing::warn!(
                    target: "regia_framework",
                    "component type {short_path} in the save is not registered; skipping"
                );
                continue;
            };
            let value = codec
                .from_postcard_typed(bytes, reg)
                .map_err(|e| SaveError::Deserialize(format!("component {short_path}: {e}")))?;
            if let Some(reflect_component) = reg.data::<regia_ecs::reflect::ReflectComponent>() {
                components.push((reflect_component, value));
            }
        }
        staged.push(components);
    }

    // Then every resource payload, through its registered entry.
    let collected = world.get_resource::<SaveableResources>().map(|list| {
        list.entries
            .iter()
            .map(|entry| (entry.type_path, entry.restore, entry.check))
            .collect::<Vec<_>>()
    });
    let section_keys: Vec<&'static str> = world
        .get_resource::<SaveSections>()
        .map(|sections| sections.keys().collect())
        .unwrap_or_default();
    let mut restores = Vec::new();
    for saved in &raw.resources {
        if section_keys.contains(&saved.type_path.as_str()) {
            continue;
        }
        // A save written before the rename to Regia Engine spells engine crates in a resource's
        // type path (a generic argument such as `Slots<…::Guid>`) with the old names (ADR-1429).
        let saved_path = regia_core::brand::current_type_path(&saved.type_path);
        let Some((restore, check)) = collected
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .find(|(type_path, _, _)| *type_path == saved.type_path || *type_path == saved_path)
            .map(|(_, restore, check)| (*restore, *check))
        else {
            tracing::warn!(
                target: "regia_framework",
                "resource {} in the save is not registered as saveable; skipping",
                saved.type_path
            );
            continue;
        };
        check(registry, &saved.data)?;
        restores.push((restore, saved));
    }

    // Every entity reference names an entry of the id table (ADR-3239).
    let refs = crate::save_entity_refs::LoadRefs::check(raw, &mut staged, registry)?;

    // Everything decodes: despawn existing saveable entities so a load never duplicates world
    // state, then spawn the saved ones. Their non-saveable children are not part of the save and
    // survive (ADR-7601): detached first, re-parented below.
    let survivors = detach_unsaved_children(world);
    let mut to_despawn: Vec<Entity> = world
        .query_filtered::<Entity, With<Saveable>>()
        .iter(world)
        .collect();
    to_despawn.sort();
    for entity in to_despawn {
        // A saveable child already went with its saveable parent (every child left under a
        // saveable entity is saveable now). `despawn_tree` is stack-safe for deep hierarchies.
        if world.entities().contains(entity) {
            regia_scene::hierarchy::despawn_tree(world, entity);
        }
    }
    // All saved entities exist before any reference is remapped or any component inserted.
    let spawned: Vec<Entity> = staged.iter().map(|_| world.spawn(Saveable).id()).collect();
    refs.remap(world, &spawned, &mut staged, registry);
    for (entity, components) in spawned.iter().copied().zip(staged) {
        let Ok(mut entity_cmd) = world.get_entity_mut(entity) else {
            continue;
        };
        for (reflect_component, value) in components {
            reflect_component.insert(&mut entity_cmd, value.as_partial_reflect(), registry);
        }
    }
    reattach_unsaved_children(world, &spawned, survivors);
    for (restore, saved) in restores {
        if let Err(e) = restore(world, registry, &saved.data) {
            tracing::warn!(
                target: "regia_framework",
                "cannot deserialize resource {}: {e}",
                saved.type_path
            );
        }
    }

    // Sections last: they see the restored world.
    read_sections(world, raw);

    Ok(())
}

/// The stable id a load matches a parent by: `(LevelMember level or 0, EntityGuid)`, the key of
/// `SaveEntityId::Level`. Runtime entities (no `EntityGuid`) have none.
fn stable_key(world: &World, entity: Entity) -> Option<(u128, u128)> {
    let guid = world.get::<regia_scene::components::EntityGuid>(entity)?;
    let level = world
        .get::<regia_scene::level::LevelMember>(entity)
        .map_or(0, |member| member.0);
    Some((level, guid.0.as_u128()))
}

/// A non-`Saveable` child a load detached from a `Saveable` parent (ADR-7601).
struct UnsavedChild {
    entity: Entity,
    /// The old parent's stable id, if it had one.
    parent_key: Option<(u128, u128)>,
    /// The child's local `Transform` under the old parent, restored when it is re-parented.
    local: Option<regia_scene::transform::Transform>,
}

/// Detaches every non-`Saveable` direct child of a `Saveable` entity, so the parent's despawn
/// (which takes its `Children` subtree) does not take it. The child becomes a root at the world
/// pose it had (its `GlobalTransform` baked into its `Transform`, as a level unload does); its
/// own subtree rides along. Entity order, so the result is deterministic.
fn detach_unsaved_children(world: &mut World) -> Vec<UnsavedChild> {
    let mut found: Vec<(Entity, Entity)> = world
        .query_filtered::<(Entity, &regia_ecs::ecs::hierarchy::ChildOf), Without<Saveable>>()
        .iter(world)
        .map(|(entity, link)| (entity, link.parent()))
        .filter(|(_, parent)| world.get::<Saveable>(*parent).is_some())
        .collect();
    found.sort();
    let mut out = Vec::with_capacity(found.len());
    for (entity, parent) in found {
        let local = world.get::<regia_scene::transform::Transform>(entity).copied();
        // Without a `GlobalTransform` no propagation has placed the child yet: nothing to keep.
        let baked = (local.is_some()
            && world
                .get::<regia_scene::transform::GlobalTransform>(entity)
                .is_some())
        .then(|| regia_scene::hierarchy::local_under_parent(world, entity, None));
        let parent_key = stable_key(world, parent);
        let Ok(mut entity_mut) = world.get_entity_mut(entity) else {
            continue;
        };
        if let Some(baked) = baked {
            entity_mut.insert(baked);
        }
        entity_mut.remove::<regia_ecs::ecs::hierarchy::ChildOf>();
        out.push(UnsavedChild {
            entity,
            parent_key,
            local,
        });
    }
    out
}

/// Re-parents each detached child to the entity the load spawned with its old parent's stable id
/// (the first such entity in save order), restoring its local `Transform`, so an attachment
/// follows the loaded parent. A child without a match stays a root where it was.
fn reattach_unsaved_children(world: &mut World, spawned: &[Entity], survivors: Vec<UnsavedChild>) {
    if survivors.is_empty() {
        return;
    }
    let mut by_key: std::collections::HashMap<(u128, u128), Entity> = std::collections::HashMap::new();
    for entity in spawned {
        if let Some(key) = stable_key(world, *entity) {
            by_key.entry(key).or_insert(*entity);
        }
    }
    for child in survivors {
        let Some(parent) = child.parent_key.and_then(|key| by_key.get(&key).copied()) else {
            continue;
        };
        if world.get_entity(child.entity).is_err() {
            continue;
        }
        if regia_scene::hierarchy::would_create_cycle(world, child.entity, parent) {
            tracing::warn!(
                target: "regia_framework",
                "load: {:?} stays a root; re-parenting it under {parent:?} would close a loop",
                child.entity
            );
            continue;
        }
        let Ok(mut entity_mut) = world.get_entity_mut(child.entity) else {
            continue;
        };
        if let Some(local) = child.local {
            entity_mut.insert(local);
        }
        entity_mut.insert(regia_ecs::ecs::hierarchy::ChildOf(parent));
    }
}

// ---------------------------------------------------------------------------
// Atomic write helper
// ---------------------------------------------------------------------------

/// Writes `bytes` to `path` atomically: write a temp file of this call's own, fsync it, rename
/// over the target, fsync the directory (best effort).
///
/// The temp file is `<path>.tmp.<pid>.<n>`, created new (never opened through an existing entry
/// or a symbolic link), so two writers of one slot (an autosave and a manual save, or a game and
/// the editor) never share it, and a failed fsync fails the save instead of reporting success.
pub fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> Result<(), SaveError> {
    let temp = write_temp(path, bytes)?;
    if let Err(e) = std::fs::rename(&temp, path) {
        let _ = std::fs::remove_file(&temp);
        return Err(SaveError::Io(e, temp));
    }
    finish_write(path);
    Ok(())
}

/// [`write_atomic`] for a save slot that keeps one previous good generation (ADR-3102): the new
/// bytes go to a synced temp file first; a current file that passes its container check
/// (`frame_is_intact`) is staged beside `<slot>.prev` (a hard link, else a synced copy); then the
/// temp file is renamed over the slot, and only after that the staged copy replaces
/// `<slot>.prev`. A failed save therefore leaves both files as they were, and the slot never
/// lacks its current file (ADR-5573). Returns whether the old file was kept as `.prev`.
///
/// # Errors
/// The IO error of the temp write or the final rename (`SaveError::Io`), disk full and
/// permission denied included. Nothing retries.
pub fn write_atomic_keep_previous(path: &std::path::Path, bytes: &[u8]) -> Result<bool, SaveError> {
    write_atomic_keep_previous_with(path, bytes, &fs_rename)
}

fn fs_rename(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
    std::fs::rename(from, to)
}

/// [`write_atomic_keep_previous`] with the rename injected (tests fail the final rename).
pub(crate) fn write_atomic_keep_previous_with(
    path: &std::path::Path,
    bytes: &[u8],
    rename: &dyn Fn(&std::path::Path, &std::path::Path) -> std::io::Result<()>,
) -> Result<bool, SaveError> {
    let temp = write_temp(path, bytes)?;
    let previous = previous_path(path);
    let current_good = std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file())
        && read_save_bytes(path).is_ok_and(|current| frame_is_intact(&current));
    let staged = if current_good {
        match stage_previous(path, &previous) {
            Ok(staged) => Some(staged),
            // The save matters more than the backup: carry on without one.
            Err(e) => {
                tracing::warn!(
                    target: "regia_framework",
                    "save {}: cannot keep the previous copy as {}: {e}",
                    path.display(),
                    previous.display()
                );
                None
            }
        }
    } else {
        None
    };
    if let Err(e) = rename(&temp, path) {
        let _ = std::fs::remove_file(&temp);
        if let Some(staged) = &staged {
            let _ = std::fs::remove_file(staged);
        }
        return Err(SaveError::Io(e, temp));
    }
    let mut rotated = false;
    if let Some(staged) = staged {
        match rename(&staged, &previous) {
            Ok(()) => rotated = true,
            Err(e) => {
                let _ = std::fs::remove_file(&staged);
                tracing::warn!(
                    target: "regia_framework",
                    "save {}: cannot keep the previous copy as {}: {e}",
                    path.display(),
                    previous.display()
                );
            }
        }
    }
    finish_write(path);
    Ok(rotated)
}

/// A uniquely named copy of the current file `path` beside `previous` (`<slot>.prev.tmp.<pid>.<n>`):
/// a hard link when the file system has them, else a copy synced to disk.
fn stage_previous(path: &std::path::Path, previous: &std::path::Path) -> std::io::Result<PathBuf> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);

    let mut name = previous
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_default();
    name.push(format!(
        ".tmp.{}.{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let staged = previous.with_file_name(name);
    if std::fs::hard_link(path, &staged).is_ok() {
        return Ok(staged);
    }
    let copied = std::fs::copy(path, &staged)
        .and_then(|_| std::fs::OpenOptions::new().write(true).open(&staged))
        .and_then(|file| file.sync_all());
    if let Err(e) = copied {
        let _ = std::fs::remove_file(&staged);
        return Err(e);
    }
    Ok(staged)
}

/// Removes a slot's current file and its previous generation (a deliberate delete). `Ok(false)`
/// when neither existed.
pub(crate) fn delete_slot_files(path: &std::path::Path) -> Result<bool, SaveError> {
    let mut existed = false;
    for file in [path.to_path_buf(), previous_path(path)] {
        match std::fs::remove_file(&file) {
            Ok(()) => existed = true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(SaveError::Io(e, file)),
        }
    }
    Ok(existed)
}

/// After a successful rename: removes a plain-file orphan of the old fixed temp name and syncs
/// the directory (best effort).
fn finish_write(path: &std::path::Path) {
    // An orphan of the old fixed temp name (a writer that crashed before this release) is
    // removed when it is a plain file; a link or a directory there is left alone.
    let legacy = path.with_extension("save.tmp");
    if std::fs::symlink_metadata(&legacy).is_ok_and(|m| m.file_type().is_file()) {
        let _ = std::fs::remove_file(&legacy);
    }
    if let Some(parent) = path.parent()
        && let Ok(dir) = std::fs::File::open(parent)
    {
        let _ = dir.sync_all();
    }
}

/// Writes `bytes` to a new temp file of this call's own beside `path` (`<path>.tmp.<pid>.<n>`)
/// and syncs it. Returns the temp path; on failure the temp file is removed.
fn write_temp(path: &std::path::Path, bytes: &[u8]) -> Result<PathBuf, SaveError> {
    use std::io::Write as _;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| SaveError::Io(e, parent.to_path_buf()))?;
    }
    let mut name = path
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_default();
    name.push(format!(
        ".tmp.{}.{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let temp = path.with_file_name(name);
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .and_then(|mut file| {
            file.write_all(bytes)?;
            file.sync_all()
        });
    if let Err(e) = written {
        let _ = std::fs::remove_file(&temp);
        return Err(SaveError::Io(e, temp));
    }
    Ok(temp)
}

/// Reads only the header from a save file ([`read_header_streaming`]).
fn read_header(path: &std::path::Path) -> Option<SaveHeader> {
    read_header_streaming(path).ok().flatten()
}

/// How many payload bytes the header is decoded from: the format version, both version strings,
/// the timestamp and the thumbnail flag lead the payload.
const HEADER_PREFIX_BYTES: usize = 64 * 1024;

/// Chunk size of the streamed CRC-32 check.
const HEADER_CRC_CHUNK: usize = 64 * 1024;

/// The leading fields of the save payload (`SaveFileRaw`), in its field order; postcard writes an
/// `Option` as a one-byte tag (0 or 1) before its value.
#[derive(Deserialize)]
struct HeaderPrefix {
    /// The payload's own copy of the format version (the container's, bytes 4..8, is reported).
    _version: u32,
    game_version: String,
    engine_version: String,
    timestamp_secs: f64,
    thumbnail_tag: u8,
}

/// The header of the save file at `path` for a slot list, holding at most
/// `HEADER_PREFIX_BYTES` + `HEADER_CRC_CHUNK` bytes in memory whatever the file's size (threat
/// model finding 18, ADR-3285). A file over [`MAX_SAVE_FILE_BYTES`] is refused by its size before a
/// byte is read; otherwise the CRC-32 footer is checked by streaming the payload in fixed chunks,
/// and the header fields are decoded from the payload's first bytes. Unlike
/// [`SaveHeader::from_bytes`], the entities and resources after the header are not decoded.
///
/// `Ok(None)` when the file is not a valid save (bad magic, length or CRC, a header that does not
/// decode, or a file that changed length while it was read).
///
/// # Errors
/// The file cannot be opened or read (`SaveError::Io`, kind `NotFound` when it is missing), or it
/// is larger than [`MAX_SAVE_FILE_BYTES`] (`SaveError::Corrupt`).
pub(crate) fn read_header_streaming(path: &std::path::Path) -> Result<Option<SaveHeader>, SaveError> {
    use std::io::Read as _;
    let io = |e| SaveError::Io(e, path.to_path_buf());
    let file = std::fs::File::open(path).map_err(io)?;
    let len = file.metadata().map_err(io)?.len();
    if len > MAX_SAVE_FILE_BYTES {
        return Err(SaveError::Corrupt(format!(
            "{} is larger than the {MAX_SAVE_FILE_BYTES} bytes a save file may be",
            path.display()
        )));
    }
    if len < 12 {
        return Ok(None);
    }
    let mut reader = std::io::BufReader::with_capacity(HEADER_CRC_CHUNK, file.take(len));
    let mut lead = [0u8; 8];
    if reader.read_exact(&mut lead).is_err() || &lead[..4] != SAVE_MAGIC {
        return Ok(None);
    }
    let version = u32::from_le_bytes([lead[4], lead[5], lead[6], lead[7]]);
    let mut payload_left = len - 12;
    let mut prefix = Vec::with_capacity(
        usize::try_from(payload_left).map_or(HEADER_PREFIX_BYTES, |n| n.min(HEADER_PREFIX_BYTES)),
    );
    let mut crc = crc32_begin();
    let mut chunk = vec![0u8; HEADER_CRC_CHUNK];
    while payload_left > 0 {
        let want = usize::try_from(payload_left).map_or(HEADER_CRC_CHUNK, |n| n.min(HEADER_CRC_CHUNK));
        let read = reader.read(&mut chunk[..want]).map_err(io)?;
        if read == 0 {
            // Shorter than its length said: it changed while it was read.
            return Ok(None);
        }
        let bytes = &chunk[..read];
        crc = crc32_update(crc, bytes);
        let room = HEADER_PREFIX_BYTES - prefix.len();
        prefix.extend_from_slice(&bytes[..read.min(room)]);
        payload_left -= read as u64;
    }
    let mut footer = [0u8; 4];
    if reader.read_exact(&mut footer).is_err() || crc32_finish(crc) != u32::from_le_bytes(footer) {
        return Ok(None);
    }
    let Ok((head, _)) = postcard::take_from_bytes::<HeaderPrefix>(&prefix) else {
        return Ok(None);
    };
    let has_thumbnail = match head.thumbnail_tag {
        0 => false,
        1 => true,
        _ => return Ok(None),
    };
    Ok(Some(SaveHeader {
        version,
        game_version: head.game_version,
        engine_version: head.engine_version,
        timestamp_secs: head.timestamp_secs,
        has_thumbnail,
    }))
}

impl SaveHeader {
    /// The header of save-file bytes (the [`SaveGame::encode`] container), or `None` when the
    /// magic, the length or the CRC-32 check fails, or the payload does not decode.
    pub fn from_bytes(bytes: &[u8]) -> Option<SaveHeader> {
        if bytes.len() < 12 || &bytes[..4] != SAVE_MAGIC {
            return None;
        }
        let version = u32::from_le_bytes(bytes[4..8].try_into().ok()?);
        let crc_stored = u32::from_le_bytes(bytes[bytes.len() - 4..].try_into().ok()?);
        let payload = &bytes[8..bytes.len() - 4];
        if crc32(payload) != crc_stored {
            return None;
        }
        // Format 1's layout is a prefix of format 2's: the header fields read the same.
        let (game_version, engine_version, timestamp_secs, has_thumbnail) =
            crate::save_entity_refs::decode_header_fields(payload)?;
        Some(SaveHeader {
            version,
            game_version,
            engine_version,
            timestamp_secs,
            has_thumbnail,
        })
    }
}

// ---------------------------------------------------------------------------
// CRC-32
// ---------------------------------------------------------------------------

/// CRC-32 (IEEE 802.3) complement of `data`. Used as a file-integrity footer (saves and `.frec`
/// match records).
pub(crate) fn crc32(data: &[u8]) -> u32 {
    crc32_finish(crc32_update(crc32_begin(), data))
}

/// The CRC-32 state before any byte ([`crc32_update`], then [`crc32_finish`]).
pub(crate) fn crc32_begin() -> u32 {
    0xFFFF_FFFF
}

/// The CRC-32 value of a state.
pub(crate) fn crc32_finish(crc: u32) -> u32 {
    !crc
}

/// Feeds `data` into a CRC-32 state, so a file can be checked in chunks.
pub(crate) fn crc32_update(mut crc: u32, data: &[u8]) -> u32 {
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xEDB8_8320;
            } else {
                crc >>= 1;
            }
        }
    }
    crc
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Save/load failures.
#[derive(thiserror::Error, Debug)]
pub enum SaveError {
    /// Serialisation failure.
    #[error("save serialization error: {0}")]
    Serialize(String),
    /// Deserialisation failure.
    #[error("save deserialization error: {0}")]
    Deserialize(String),
    /// IO failure.
    #[error("save IO error at {1}: {0}")]
    Io(std::io::Error, PathBuf),
    /// Corrupted file.
    #[error("save file corrupted: {0}")]
    Corrupt(String),
}

// ---------------------------------------------------------------------------
// Plugin
// ---------------------------------------------------------------------------

/// Hooks save-game lifecycle into the app: initialises the save directory and the saveable
/// resource list, and autosaves once when the app enters `Suspended`.
pub struct SavePlugin {
    /// The active save slot for autosave (default: `"Auto"`).
    pub autosave_slot: String,
}

/// Remembers whether this suspension has already been written.
///
/// [`autosave_on_suspend`] runs in the [`Lifecycle`](regia_app::Lifecycle) schedule, which
/// is the only time [`regia_platform::AppLifecycle::last_event`] is `Suspended`. The latch
/// still matters: a second `Suspended` transition without a `Resumed` in between (or a
/// caller that leaves the event set) must not write the slot again.
#[derive(Resource, Debug, Default)]
struct AutosaveLatch {
    /// `true` after this suspension has been saved (or the attempt has been logged).
    written: bool,
}

impl Default for SavePlugin {
    fn default() -> Self {
        Self {
            autosave_slot: "Auto".into(),
        }
    }
}

impl Plugin for SavePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<SaveDirectory>()
            .init_resource::<SaveableResources>()
            .init_resource::<SaveSections>()
            .init_resource::<SaveMigrations>()
            .init_resource::<AutosaveLatch>()
            .register_type::<Saveable>()
            .insert_resource(AutosaveSlot(self.autosave_slot.clone()))
            .add_systems(Lifecycle, autosave_on_suspend);
    }
}

/// The slot [`autosave_on_suspend`] writes. Installed by [`SavePlugin`].
#[derive(Resource, Debug, Clone)]
struct AutosaveSlot(String);

/// One save when the app enters `Suspended`, on every platform.
///
/// The spec names Android because that is where the OS suspends the process. The runner
/// applies that transition with [`regia_platform::apply_transition`], which runs this
/// schedule while `last_event` is `Suspended` and then stops calling `App::update` until
/// `Resumed`. The hook therefore lives here, not in `Update` and not behind `cfg(android)`,
/// so a host test can drive the same edge. A later `Suspended` without a `Resumed` does not
/// write again; `Resumed` (state back to running) arms the next edge.
#[tracing::instrument(skip_all, target = "regia_framework", name = "autosave_on_suspend")]
fn autosave_on_suspend(world: &mut World) {
    let Some(lifecycle) = world.get_resource::<regia_platform::AppLifecycle>().cloned() else {
        return;
    };
    let edge = lifecycle.last_event == Some(regia_platform::LifecycleEvent::Suspended);
    if !edge {
        // `Resumed` (and any other transition that leaves `Suspended`) arms the next edge.
        // Staying suspended without a new `Suspended` event must not clear the latch: that
        // would turn the following frame into another save.
        if lifecycle.state != regia_platform::LifecycleState::Suspended
            && let Some(mut latch) = world.get_resource_mut::<AutosaveLatch>()
        {
            latch.written = false;
        }
        return;
    }
    if world
        .get_resource::<AutosaveLatch>()
        .is_some_and(|latch| latch.written)
    {
        return;
    }
    let slot_name = world
        .get_resource::<AutosaveSlot>()
        .map(|slot| slot.0.clone())
        .unwrap_or_else(|| "Auto".to_owned());
    // With a save store installed (spec 91), the autosave goes where every other save goes.
    let result = if world.contains_resource::<crate::save_flow::SaveStore>() {
        crate::save_flow::save_to_store(world, &slot_name).map(|_| ())
    } else {
        let dir = world.resource::<SaveDirectory>().clone();
        let slot = SaveGame::slot(&dir, &slot_name);
        SaveGame::save(&slot, world, &SaveSpec::default()).map(|_| ())
    };
    if let Err(err) = result {
        tracing::error!(target: "regia_framework", "autosave failed: {err}");
    }
    if let Some(mut latch) = world.get_resource_mut::<AutosaveLatch>() {
        latch.written = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Saves live under the platform's user directory; only a platform without one falls back to
    /// the working directory (Android's is `/`, read-only: the device autosave failed there).
    #[test]
    fn the_default_save_directory_is_under_the_user_directory() {
        let user = PathBuf::from("/data/user/0/com.example.game/files");
        assert_eq!(default_save_dir(Some(user.clone())), user.join("saves"));
        assert_eq!(default_save_dir(None), PathBuf::from("./saves"));
        let dir = SaveDirectory::default().0;
        assert!(dir.ends_with("saves"), "{}", dir.display());
        if let Some(user) = regia_platform::paths::user_dir() {
            assert!(dir.starts_with(user), "{}", dir.display());
        }
    }
    use regia_core::math::Vec3;
    use regia_scene::Transform;

    /// Test component: a reflected, saveable health value.
    #[derive(Component, Reflect, Default, Clone, Debug, PartialEq)]
    #[reflect(Component, Default)]
    struct Health {
        hp: f32,
    }

    /// Test resource: a reflected, saveable score.
    #[derive(Resource, Reflect, Default, Clone, Debug, PartialEq)]
    #[reflect(Resource, Default)]
    struct Score(u32);

    impl SaveableResource for Score {}

    /// Another resource the game does NOT register: must never appear in a save.
    #[derive(Resource, Reflect, Default, Clone, Debug, PartialEq)]
    #[reflect(Resource, Default)]
    struct Ephemeral(u32);

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("regia-save-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    /// A minimal app with the save plugin and the test types registered.
    fn app() -> App {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, SavePlugin::default()))
            .register_type::<Health>()
            .register_type::<Transform>()
            .register_saveable_resource::<Score>()
            .register_type::<Ephemeral>();
        app
    }

    #[test]
    fn a_failed_final_rename_keeps_both_generations() {
        // Three saves: G-1, G0, then G1 whose final rename fails (an antivirus holding the temp
        // file on Windows). The slot must keep G0 current and G-1 as `.prev`.
        let dir = regia_test_utils::unique_temp_dir("save-keep-prev-fail");
        let slot = SaveSlot {
            name: "Keep".into(),
            path: dir.join("Keep.save"),
            header: None,
        };
        let mut app = app();
        let mut bytes_of = |score: u32| {
            app.insert_resource(Score(score));
            SaveGame::save(&slot, app.world_mut(), &SaveSpec::default()).expect("save");
            std::fs::read(&slot.path).expect("read current")
        };
        let older = bytes_of(1);
        let current = bytes_of(2);
        assert_eq!(std::fs::read(slot.previous_path()).expect("prev"), older);
        let newer = bytes_of(3);
        // Restore G0 / G-1 by hand, then fail the final rename of G1.
        std::fs::write(&slot.path, &current).expect("current");
        std::fs::write(slot.previous_path(), &older).expect("prev");
        let target = slot.path.clone();
        let failing = |from: &std::path::Path, to: &std::path::Path| {
            if to == target {
                Err(std::io::Error::other("injected"))
            } else {
                std::fs::rename(from, to)
            }
        };
        let result = write_atomic_keep_previous_with(&slot.path, &newer, &failing);
        assert!(matches!(result, Err(SaveError::Io(..))), "{result:?}");
        assert_eq!(std::fs::read(&slot.path).expect("current kept"), current);
        assert_eq!(
            std::fs::read(slot.previous_path()).expect("previous kept"),
            older,
            "the older generation survives a failed save"
        );
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .expect("dir")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp."))
            .collect();
        assert!(
            leftovers.is_empty(),
            "no temp or staged file is left: {leftovers:?}"
        );
        // A save that works rotates as before.
        assert!(write_atomic_keep_previous(&slot.path, &newer).expect("save"));
        assert_eq!(std::fs::read(slot.previous_path()).expect("prev"), current);
        assert_eq!(std::fs::read(&slot.path).expect("current"), newer);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_then_load_round_trips_components_and_resources() {
        let dir = temp_dir("world-roundtrip");
        let slot = SaveSlot {
            name: "Test".into(),
            path: dir.join("Test.save"),
            header: None,
        };
        let mut app = app();

        // World state to save: one saveable entity with component values, one resource.
        app.world_mut().spawn((
            Saveable,
            Health { hp: 73.5 },
            Transform {
                translation: Vec3::new(1.0, 2.0, 3.0),
                ..Default::default()
            },
        ));
        // Not saveable: must not appear in the save or survive the load.
        app.world_mut().spawn((Health { hp: 1.0 },));
        app.insert_resource(Score(42));
        app.insert_resource(Ephemeral(9));

        let saved = SaveGame::save(&slot, app.world_mut(), &SaveSpec::default()).expect("save succeeds");
        assert_eq!(saved.entity_count, 1, "only the Saveable entity is saved");
        assert_eq!(saved.resource_count, 1, "only the registered resource is saved");
        assert!(saved.bytes_written > 12, "file size is reported");

        // Clear the world: despawn the game entities and reset the resources. (Hidden resource
        // entities are engine-internal in this ECS and are not game state.)
        let to_clear: Vec<Entity> = app
            .world_mut()
            .query_filtered::<Entity, With<Health>>()
            .iter(app.world_mut())
            .collect();
        for entity in to_clear {
            app.world_mut().despawn(entity);
        }
        app.insert_resource(Score(0));
        app.insert_resource(Ephemeral(0));
        assert_eq!(app.world_mut().query::<&Health>().iter(app.world()).count(), 0);

        let loaded = SaveGame::load(&slot, app.world_mut(), &SaveSpec::default()).expect("load succeeds");
        assert_eq!(loaded.entity_count, 1);
        assert_eq!(loaded.resource_count, 1);
        assert_eq!(loaded.bytes_written, saved.bytes_written);

        // The entity is back with the same component VALUES.
        let world = app.world_mut();
        let mut q = world.query::<(&Health, &Transform)>();
        let (hp, translation) = q
            .iter(world)
            .map(|(h, t)| (h.hp, t.translation))
            .next()
            .expect("the saved entity is restored with its components");
        assert_eq!(hp, 73.5);
        assert_eq!(translation, Vec3::new(1.0, 2.0, 3.0));
        // And only one Health exists: the unmarked entity stayed despawned.
        let world = app.world_mut();
        let mut all = world.query::<&Health>();
        assert_eq!(all.iter(world).count(), 1);
        // The entity is marked saveable again.
        assert_eq!(
            app.world_mut()
                .query_filtered::<Entity, With<Saveable>>()
                .iter(app.world())
                .count(),
            1
        );

        // The resource value is restored; the unregistered resource is untouched.
        assert_eq!(app.world().resource::<Score>().0, 42);
        assert_eq!(app.world().resource::<Ephemeral>().0, 0);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn migration_changes_a_component_field_on_the_real_world_path() {
        let dir = temp_dir("world-migrate");
        let path = dir.join("Old.save");

        let mut app = app();
        // The game's migration from format version 0: health used to be stored in whole
        // points; version 1 stores tenths. The hook rewrites the component payload through
        // the registry.
        let mut migrations = SaveMigrations::default();
        migrations.register(0, |ctx, raw| {
            let touched = raw
                .map_component(ctx.registry, "Health", &mut |value| {
                    let reflect = value.reflect_mut();
                    if let Ok(strukt) = reflect.as_struct()
                        && let Some(hp) = strukt.field_mut("hp").and_then(|f| f.try_downcast_mut::<f32>())
                    {
                        *hp *= 10.0;
                    }
                    Ok(())
                })
                .expect("migration rewrites the component");
            assert_eq!(touched, 1, "one entity carries Health");
        });
        app.insert_resource(migrations);

        // Craft a version-0 save file: an entity whose Health is 5.0 whole points.
        let registry = app.world().resource::<AppTypeRegistry>().clone();
        let registry = registry.read();
        let mut codec = ReflectCodec::new(&registry);
        let health_bytes = codec
            .to_postcard_typed(&Health { hp: 5.0 })
            .expect("encode component");
        let raw = SaveFileRaw {
            version: 0,
            game_version: "old".into(),
            engine_version: "test".into(),
            timestamp_secs: 0.0,
            thumbnail: None,
            entities: vec![SavedEntity {
                components: BTreeMap::from([("Health".to_string(), health_bytes)]),
            }],
            resources: vec![],
            entity_ids: Vec::new(),
        };
        let mut bytes = Vec::new();
        bytes.extend_from_slice(SAVE_MAGIC);
        bytes.extend_from_slice(&0u32.to_le_bytes());
        let payload = postcard::to_allocvec(&raw).expect("serialize");
        bytes.extend_from_slice(&payload);
        bytes.extend_from_slice(&crc32(&payload).to_le_bytes());
        std::fs::write(&path, &bytes).expect("write old save");

        let slot = SaveSlot {
            name: "Old".into(),
            path: path.clone(),
            header: None,
        };
        let loaded =
            SaveGame::load(&slot, app.world_mut(), &SaveSpec::default()).expect("load with migration");
        assert_eq!(loaded.entity_count, 1);

        // The migrated value reached the world: 5.0 whole points became 50.0 tenths.
        let world = app.world_mut();
        let mut q = world.query::<&Health>();
        let hp = q.iter(world).next().expect("migrated entity").hp;
        assert_eq!(hp, 50.0);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// audit-v034b-formats M3: `save_raw` stamped the header with the current format version
    /// whatever `raw.version` said, so an old payload written by a migration tool skipped its
    /// migrations on load.
    #[test]
    fn save_raw_keeps_the_payload_version_so_migrations_run() {
        let dir = temp_dir("raw-version");
        let mut app = app();
        let mut migrations = SaveMigrations::default();
        migrations.register(0, |ctx, raw| {
            raw.map_component(ctx.registry, "Health", &mut |value| {
                if let Ok(strukt) = value.reflect_mut().as_struct()
                    && let Some(hp) = strukt.field_mut("hp").and_then(|f| f.try_downcast_mut::<f32>())
                {
                    *hp *= 10.0;
                }
                Ok(())
            })
            .expect("migration rewrites the component");
        });
        app.insert_resource(migrations);
        let health_bytes = {
            let registry = app.world().resource::<AppTypeRegistry>().read();
            ReflectCodec::new(&registry)
                .to_postcard_typed(&Health { hp: 5.0 })
                .expect("encode component")
        };
        let mut raw = SaveFileRaw {
            version: 0,
            game_version: "old".into(),
            engine_version: "test".into(),
            timestamp_secs: 0.0,
            thumbnail: None,
            entities: vec![SavedEntity {
                components: BTreeMap::from([("Health".to_string(), health_bytes)]),
            }],
            resources: vec![],
            entity_ids: Vec::new(),
        };
        let slot = SaveSlot {
            name: "Old".into(),
            path: dir.join("Old.save"),
            header: None,
        };
        SaveGame::save_raw(&slot, &raw).expect("a migration tool writes an old payload");
        let bytes = std::fs::read(&slot.path).expect("read");
        assert_eq!(
            &bytes[4..8],
            &0u32.to_le_bytes(),
            "the header carries the payload's version"
        );
        SaveGame::load(&slot, app.world_mut(), &SaveSpec::default()).expect("load with migration");
        let world = app.world_mut();
        let hp = world.query::<&Health>().iter(world).next().expect("entity").hp;
        assert_eq!(hp, 50.0, "the 0 -> 1 migration ran");
        // A payload newer than this engine is refused at write time.
        raw.version = SAVE_FORMAT_VERSION + 1;
        assert!(matches!(
            SaveGame::save_raw(&slot, &raw),
            Err(SaveError::Serialize(_))
        ));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// audit-v034b-conc M1: a save file was read whole with no size cap (listing slots read every
    /// file), so one huge or sparse `.save` could exhaust memory before any check ran.
    #[test]
    fn an_oversized_save_is_refused_before_it_is_read() {
        let dir = temp_dir("oversized");
        let path = dir.join("Huge.save");
        let file = std::fs::File::create(&path).expect("create");
        // Sparse: no disk space or read time on the filesystems the tests run on.
        file.set_len(MAX_SAVE_FILE_BYTES + 1).expect("sparse length");
        drop(file);
        let slot = SaveSlot {
            name: "Huge".into(),
            path: path.clone(),
            header: None,
        };
        let mut app = app();
        match SaveGame::load(&slot, app.world_mut(), &SaveSpec::default()) {
            Err(SaveError::Corrupt(why)) => assert!(why.contains("larger than"), "{why}"),
            other => panic!("expected a size refusal, got {other:?}"),
        }
        assert!(read_header(&path).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `save_raw` of a format 1 payload that holds entity ids is refused: format 1 has no id table,
    /// so the ids (every stable entity reference) would be dropped without a word. Without ids,
    /// a format 1 payload is still written for migration tools.
    #[test]
    fn a_format_1_payload_with_entity_ids_is_refused() {
        let raw = |entity_ids| SaveFileRaw {
            version: 1,
            game_version: "old".into(),
            engine_version: "test".into(),
            timestamp_secs: 0.0,
            thumbnail: None,
            entities: vec![],
            resources: vec![],
            entity_ids,
        };
        let ids = vec![crate::save_entity_refs::SaveEntityId::Lost];
        match frame_save_file(&raw(ids)) {
            Err(SaveError::Serialize(msg)) => assert!(msg.contains("no entity id table"), "{msg}"),
            other => panic!("{other:?}"),
        }
        let bytes = frame_save_file(&raw(Vec::new())).expect("a format 1 payload without ids");
        assert_eq!(&bytes[4..8], &1u32.to_le_bytes());
    }

    #[test]
    fn a_resource_saved_under_the_legacy_crate_names_is_restored() {
        let mut app = app();
        let data = {
            let registry = app.world().resource::<AppTypeRegistry>().read();
            ReflectCodec::new(&registry)
                .to_postcard_typed(&Score(42))
                .expect("encode resource")
        };
        // This test's resource lives in an engine crate, so its path names one: a save written
        // before the rename spells it with the legacy crate name.
        let legacy = regia_core::brand::fingerprint_type_path(Score::type_path()).into_owned();
        if regia_core::brand::BRAND_RENAMED {
            assert_ne!(legacy, Score::type_path());
        }
        let raw = SaveFileRaw {
            version: SAVE_FORMAT_VERSION,
            game_version: "old".into(),
            engine_version: "0.1.29".into(),
            timestamp_secs: 0.0,
            thumbnail: None,
            entities: vec![],
            resources: vec![SavedResource {
                type_path: legacy,
                data,
            }],
            entity_ids: Vec::new(),
        };
        let bytes = frame_save_file(&raw).expect("frame");
        app.insert_resource(Score(0));
        SaveGame::decode(app.world_mut(), &bytes, &SaveSpec::default()).expect("decode");
        assert_eq!(*app.world().resource::<Score>(), Score(42));
    }

    #[test]
    fn a_save_written_by_v0129_loads() {
        // Written by the v0.1.29 engine through `SaveGame::encode` from this module's `Health`,
        // `Transform` and `Score` (scratch writer, rename audit), kept byte for byte: the
        // resource is keyed by its full path, which names the legacy crate.
        let bytes = include_bytes!("../tests/fixtures/legacy-brand/v0129.save");
        let mut app = app();
        app.insert_resource(Score(0));
        let loaded = SaveGame::decode(app.world_mut(), bytes, &SaveSpec::default()).expect("decode");
        assert_eq!((loaded.entity_count, loaded.resource_count), (1, 1));
        assert_eq!(*app.world().resource::<Score>(), Score(42));
        let world = app.world_mut();
        let mut q = world.query::<(&Health, &Transform)>();
        let (hp, translation) = q
            .iter(world)
            .map(|(h, t)| (h.hp, t.translation))
            .next()
            .expect("the saved entity is restored");
        assert_eq!(hp, 73.5);
        assert_eq!(translation, Vec3::new(1.0, 2.0, 3.0));
    }

    /// What the test section writes and what its read saw.
    #[derive(Resource, Default)]
    struct SectionLog {
        value: u8,
        read: Vec<Option<Vec<u8>>>,
    }

    fn write_log(world: &mut World) -> Result<Option<Vec<u8>>, SaveError> {
        // A section may change the world it saves: the health below is written after this.
        let mut q = world.query::<&mut Health>();
        for mut health in q.iter_mut(world) {
            health.hp += 1.0;
        }
        Ok(Some(vec![world.resource::<SectionLog>().value]))
    }

    fn read_log(world: &mut World, data: Option<&[u8]>) -> Result<(), SaveError> {
        // Sections run after the restore: the saved entity is back.
        let healths = world.query::<&Health>().iter(world).count();
        assert_eq!(healths, usize::from(data.is_some()), "read after the entities");
        world
            .resource_mut::<SectionLog>()
            .read
            .push(data.map(<[u8]>::to_vec));
        Ok(())
    }

    fn write_nothing(_: &mut World) -> Result<Option<Vec<u8>>, SaveError> {
        Ok(None)
    }

    fn write_fails(_: &mut World) -> Result<Option<Vec<u8>>, SaveError> {
        Err(SaveError::Serialize("refused".into()))
    }

    fn read_fails(_: &mut World, _: Option<&[u8]>) -> Result<(), SaveError> {
        Err(SaveError::Deserialize("refused".into()))
    }

    #[test]
    fn sections_run_on_every_path_with_their_bytes_and_keep_the_format() {
        let mut app = app();
        app.init_resource::<SectionLog>();
        app.world_mut().resource_mut::<SectionLog>().value = 7;
        {
            let mut sections = app.world_mut().resource_mut::<SaveSections>();
            sections.register("test::section", write_log, read_log);
            sections.register("test::section", write_nothing, read_fails);
            sections.register("test::absent", write_nothing, |_, data| {
                assert!(data.is_none());
                Ok(())
            });
            sections.register("test::broken", write_fails, read_fails);
            assert_eq!(
                sections.keys().collect::<Vec<_>>(),
                ["test::section", "test::absent", "test::broken"]
            );
        }
        app.world_mut().spawn((Saveable, Health { hp: 1.0 }));

        let (bytes, result) =
            SaveGame::encode_counted(app.world_mut(), &SaveSpec::default()).expect("a failing section warns");
        assert_eq!(result.resource_count, 1, "one section wrote bytes");
        assert_eq!(&bytes[4..8], &SAVE_FORMAT_VERSION.to_le_bytes(), "same format");

        let loaded = SaveGame::decode(app.world_mut(), &bytes, &SaveSpec::default()).expect("decode");
        assert_eq!(loaded.entity_count, 1);
        let hp = app
            .world_mut()
            .query::<&Health>()
            .single(app.world())
            .expect("one")
            .hp;
        assert_eq!(hp, 2.0, "the section ran before the entity was read");
        assert_eq!(app.world().resource::<SectionLog>().read, [Some(vec![7])]);

        // A save without the section still calls the read, with `None`.
        let empty = SaveFileRaw {
            version: SAVE_FORMAT_VERSION,
            game_version: "old".into(),
            engine_version: "old".into(),
            timestamp_secs: 0.0,
            thumbnail: None,
            entities: vec![],
            resources: vec![],
            entity_ids: Vec::new(),
        };
        let bytes = frame_save_file(&empty).expect("frame");
        SaveGame::decode(app.world_mut(), &bytes, &SaveSpec::default()).expect("decode");
        assert_eq!(app.world().resource::<SectionLog>().read.len(), 2);
        assert_eq!(app.world().resource::<SectionLog>().read[1], None);
    }

    #[test]
    fn a_resource_filter_selects_sections_like_resources() {
        let mut app = app();
        app.init_resource::<SectionLog>();
        app.world_mut()
            .resource_mut::<SaveSections>()
            .register("test::section", write_log, read_log);
        let spec = SaveSpec {
            resource_filter: vec!["Score".into()],
            ..SaveSpec::default()
        };
        let (_, result) = SaveGame::encode_counted(app.world_mut(), &spec).expect("encode");
        assert_eq!(result.resource_count, 0);
        let spec = SaveSpec {
            resource_filter: vec!["section".into()],
            ..SaveSpec::default()
        };
        let (_, result) = SaveGame::encode_counted(app.world_mut(), &spec).expect("encode");
        assert_eq!(result.resource_count, 1);
    }

    #[test]
    fn register_saveable_resource_is_idempotent_and_collects() {
        let mut app = app();
        app.register_saveable_resource::<Score>();
        let list = app.world().resource::<SaveableResources>();
        let paths: Vec<&str> = list.type_paths().collect();
        assert_eq!(paths, vec![Score::type_path()], "registered once");

        // The extension registered the type for reflection.
        let registry = app.world().resource::<AppTypeRegistry>();
        let registry = registry.read();
        assert!(
            registry
                .get(std::any::TypeId::of::<Score>())
                .is_some_and(|r| r.data::<regia_ecs::reflect::ReflectResource>().is_some())
        );
    }

    #[test]
    fn save_and_load_round_trip_via_raw() {
        let dir = temp_dir("roundtrip");
        let slot = SaveSlot {
            name: "Test".into(),
            path: dir.join("Test.save"),
            header: None,
        };

        let raw = SaveFileRaw {
            version: SAVE_FORMAT_VERSION,
            game_version: "1.0".into(),
            engine_version: "0.1".into(),
            timestamp_secs: 12345.0,
            thumbnail: None,
            entities: vec![SavedEntity {
                components: {
                    let mut m = BTreeMap::new();
                    m.insert("my_game::Health".into(), vec![1, 2, 3]);
                    m
                },
            }],
            resources: vec![SavedResource {
                type_path: "my_game::Score".into(),
                data: vec![42, 0, 0, 0],
            }],
            entity_ids: Vec::new(),
        };

        let saved = SaveGame::save_raw(&slot, &raw).expect("save works");
        assert!(saved.bytes_written > 12);
        assert!(slot.path.exists());

        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        let loaded = SaveGame::load_raw(&slot, app.world_mut()).expect("load works");
        assert_eq!(loaded.game_version, "1.0");
        assert_eq!(loaded.entities.len(), 1);
        assert_eq!(loaded.resources.len(), 1);
        assert_eq!(loaded.resources[0].data, vec![42, 0, 0, 0]);

        std::fs::remove_file(&slot.path).ok();
    }

    #[test]
    fn v1_fixture_still_loads() {
        // A checked-in minimal v1 save (magic + version + postcard payload + CRC) must keep loading.
        let bytes = include_bytes!("../tests/fixtures/empty.save");
        assert_eq!(&bytes[..4], SAVE_MAGIC, "bad magic");
        let version = u32::from_le_bytes(bytes[4..8].try_into().expect("4 bytes"));
        assert_eq!(version, 1, "v1 fixture");
        let payload = &bytes[8..bytes.len() - 4];
        let crc_stored = u32::from_le_bytes(bytes[bytes.len() - 4..].try_into().expect("4 bytes"));
        assert_eq!(crc32(payload), crc_stored, "CRC mismatch");
        // Format 1 has no entity id table: it decodes with its own layout (ADR-3239).
        let raw: SaveFileRaw =
            crate::save_entity_refs::decode_payload(1, payload).expect("decode v1 payload");
        assert!(raw.entity_ids.is_empty());
        assert_eq!(raw.version, 1);
        assert!(raw.entities.is_empty());
        assert!(raw.resources.is_empty());
    }

    #[test]
    fn atomic_write_survives_crash() {
        let dir = temp_dir("atomic");
        let path = dir.join("Crash.save");
        let tmp = dir.join("Crash.save.tmp");

        write_atomic(&path, b"hello save").expect("write works");
        assert!(path.exists());
        assert!(!tmp.exists(), "temp file renamed away");

        std::fs::write(&tmp, b"orphan").expect("write orphan");
        assert!(tmp.exists());
        write_atomic(&path, b"new data").expect("overwrite works");
        assert!(!tmp.exists(), "old temp cleaned up");
        let contents = std::fs::read_to_string(&path).expect("read");
        assert_eq!(contents, "new data");

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn list_slots_finds_all_saves() {
        let dir = SaveDirectory(temp_dir("list"));
        std::fs::create_dir_all(&dir.0).ok();

        for name in ["Auto", "Slot1", "Quick"] {
            let slot = SaveGame::slot(&dir, name);
            let raw = SaveFileRaw {
                version: SAVE_FORMAT_VERSION,
                game_version: "test".into(),
                engine_version: "test".into(),
                timestamp_secs: 0.0,
                thumbnail: None,
                entities: vec![],
                resources: vec![],
                entity_ids: Vec::new(),
            };
            let mut bytes = Vec::new();
            bytes.extend_from_slice(SAVE_MAGIC);
            bytes.extend_from_slice(&SAVE_FORMAT_VERSION.to_le_bytes());
            let payload = postcard::to_allocvec(&raw).expect("serialize");
            let crc = crc32(&payload);
            bytes.extend_from_slice(&payload);
            bytes.extend_from_slice(&crc.to_le_bytes());
            std::fs::write(&slot.path, bytes).expect("write");
        }

        let slots = SaveGame::list_slots(&dir);
        assert_eq!(slots.len(), 3);
        let names: Vec<&str> = slots.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["Auto", "Quick", "Slot1"]);
        assert!(slots.iter().all(|s| s.header.is_some()));

        std::fs::remove_dir_all(&dir.0).ok();
    }

    #[test]
    fn crc_mismatch_is_corrupt() {
        let dir = temp_dir("crc");
        let path = dir.join("Bad.save");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(SAVE_MAGIC);
        bytes.extend_from_slice(&SAVE_FORMAT_VERSION.to_le_bytes());
        bytes.extend_from_slice(b"this is not valid postcard data!!!");
        bytes.extend_from_slice(&0xDEAD_BEEF_u32.to_le_bytes());
        std::fs::write(&path, &bytes).expect("write");

        let slot = SaveSlot {
            name: "Bad".into(),
            path: path.clone(),
            header: None,
        };
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        let err = SaveGame::load_raw(&slot, app.world_mut()).expect_err("corrupt save must fail");
        assert!(matches!(err, SaveError::Corrupt(_)), "should be corrupt: {err}");
        // list_slots still reports the slot, but with no header: it failed validation.
        let slots = SaveGame::list_slots(&SaveDirectory(dir.clone()));
        assert_eq!(slots.len(), 1, "the damaged slot is listed");
        assert!(slots[0].header.is_none(), "but its header failed validation");

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn bad_magic_is_corrupt() {
        let dir = temp_dir("magic");
        let path = dir.join("Bad.save");
        std::fs::write(&path, b"XXXX1234extra").expect("write");
        let slot = SaveSlot {
            name: "Bad".into(),
            path,
            header: None,
        };
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        let err = SaveGame::load_raw(&slot, app.world_mut()).expect_err("bad magic must fail");
        assert!(matches!(err, SaveError::Corrupt(_)), "bad magic: {err}");
    }

    /// Save-file bytes for `raw`, written as format `version`.
    fn framed(raw: &SaveFileRaw, version: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(SAVE_MAGIC);
        bytes.extend_from_slice(&version.to_le_bytes());
        let payload = postcard::to_allocvec(raw).expect("serialize");
        bytes.extend_from_slice(&payload);
        bytes.extend_from_slice(&crc32(&payload).to_le_bytes());
        bytes
    }

    fn raw_with(entities: Vec<SavedEntity>, resources: Vec<SavedResource>, version: u32) -> SaveFileRaw {
        SaveFileRaw {
            version,
            game_version: "test".into(),
            engine_version: "test".into(),
            timestamp_secs: 0.0,
            thumbnail: None,
            entities,
            resources,
            entity_ids: Vec::new(),
        }
    }

    fn health_values(app: &mut App) -> Vec<f32> {
        let world = app.world_mut();
        let mut q = world.query::<&Health>();
        q.iter(world).map(|h| h.hp).collect()
    }

    /// Audit v034 net M1: a CRC-valid save whose component payload no longer decodes fails the
    /// load and leaves the live world as it was (it used to despawn everything, drop the
    /// component with a warning and report success, so the next autosave kept the gutted world).
    #[test]
    fn a_payload_that_does_not_decode_fails_the_load_and_keeps_the_world() {
        let mut app = app();
        app.world_mut().spawn((Saveable, Health { hp: 7.0 }));
        app.insert_resource(Score(5));
        let bad = vec![0xFF, 0xFF, 0xFF];
        {
            let registry = app.world().resource::<AppTypeRegistry>().clone();
            let registry = registry.read();
            let reg = registry.get_with_short_type_path("Health").expect("registered");
            assert!(
                ReflectCodec::new(&registry)
                    .from_postcard_typed(&bad, reg)
                    .is_err(),
                "the fixture payload must not decode"
            );
        }
        let entity = SavedEntity {
            components: BTreeMap::from([("Health".to_string(), bad.clone())]),
        };
        let bytes = framed(
            &raw_with(vec![entity], vec![], SAVE_FORMAT_VERSION),
            SAVE_FORMAT_VERSION,
        );
        let err = SaveGame::decode(app.world_mut(), &bytes, &SaveSpec::default())
            .expect_err("a payload that does not decode must fail the load");
        assert!(matches!(err, SaveError::Deserialize(_)), "{err}");
        assert_eq!(health_values(&mut app), vec![7.0], "the live entity survives");
        assert_eq!(app.world().resource::<Score>().0, 5);

        // The same for a resource payload.
        let resource = SavedResource {
            type_path: Score::type_path().to_string(),
            data: bad,
        };
        let bytes = framed(
            &raw_with(vec![], vec![resource], SAVE_FORMAT_VERSION),
            SAVE_FORMAT_VERSION,
        );
        let err = SaveGame::decode(app.world_mut(), &bytes, &SaveSpec::default())
            .expect_err("a resource that does not decode must fail the load");
        assert!(matches!(err, SaveError::Deserialize(_)), "{err}");
        assert_eq!(health_values(&mut app), vec![7.0], "the live entity survives");
        assert_eq!(app.world().resource::<Score>().0, 5);
    }

    /// Audit v034 net M2: a save older than the engine's format loads only when every step up to
    /// the current version has a migration; a missing one fails instead of applying the file as
    /// if it were current.
    #[test]
    fn a_missing_migration_step_fails_the_load() {
        let mut app = app();
        app.world_mut().spawn((Saveable, Health { hp: 7.0 }));
        let bytes = framed(&raw_with(vec![], vec![], 0), 0);
        let err = SaveGame::decode(app.world_mut(), &bytes, &SaveSpec::default())
            .expect_err("no migration from version 0 is registered");
        assert!(matches!(err, SaveError::Deserialize(_)), "{err}");
        assert!(err.to_string().contains("version 0"), "{err}");
        assert_eq!(health_values(&mut app), vec![7.0], "the live entity survives");
    }

    /// Audit v034 net L1, cook M2 and L1: the temp file is this writer's own. A stale entry where
    /// the old fixed temp name was (here a directory, which `fs::write` cannot open) does not
    /// stop the save.
    #[test]
    fn atomic_write_does_not_depend_on_a_fixed_temp_name() {
        let dir = temp_dir("atomic-stale");
        let path = dir.join("Slot.save");
        std::fs::create_dir_all(dir.join("Slot.save.tmp")).expect("stale entry");
        write_atomic(&path, b"saved").expect("the save does not use the stale name");
        assert_eq!(std::fs::read(&path).expect("read"), b"saved");
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .expect("list")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "Slot.save" && n != "Slot.save.tmp")
            .collect();
        assert!(leftovers.is_empty(), "no temp file is left behind: {leftovers:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A temp-name symlink planted next to the slot is never written through.
    #[cfg(unix)]
    #[test]
    fn atomic_write_does_not_follow_a_planted_temp_symlink() {
        let dir = temp_dir("atomic-link");
        let victim = dir.join("victim.txt");
        std::fs::write(&victim, b"keep me").expect("victim");
        std::os::unix::fs::symlink(&victim, dir.join("Slot.save.tmp")).expect("symlink");
        write_atomic(&dir.join("Slot.save"), b"saved").expect("save");
        assert_eq!(std::fs::read(&victim).expect("read"), b"keep me");
        assert_eq!(std::fs::read(dir.join("Slot.save")).expect("read"), b"saved");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Audit v034 net L3: a slot name never makes a file Windows refuses (a reserved device stem)
    /// or one `list_slots` cannot see (an empty stem), so a game saves the same on every platform.
    #[test]
    fn slot_names_are_portable_and_listable() {
        let dir = SaveDirectory(temp_dir("portable"));
        for name in ["con", "NUL", "com1", "Lpt9", "aux", "conin$", ""] {
            let slot = SaveGame::slot(&dir, name);
            let stem = slot.path.file_stem().map(|s| s.to_string_lossy().into_owned());
            let stem = stem.unwrap_or_default();
            let ext = slot.path.extension().map(|e| e.to_string_lossy().into_owned());
            assert_eq!(
                ext.as_deref(),
                Some("save"),
                "{name:?} -> {}",
                slot.path.display()
            );
            assert!(!stem.is_empty(), "{name:?} -> {}", slot.path.display());
            assert!(
                !regia_platform::is_reserved_device_name(&stem),
                "{name:?} -> {}",
                slot.path.display()
            );
            assert_eq!(slot.name, stem, "the slot is listed under its name");
        }
        // Ordinary names are unchanged.
        assert_eq!(SaveGame::slot(&dir, "console").name, "console");
        assert_eq!(SaveGame::slot(&dir, "com10").name, "com10");
        std::fs::remove_dir_all(&dir.0).ok();
    }

    #[test]
    fn slot_name_is_sanitized() {
        let dir = SaveDirectory(temp_dir("sanitize"));
        let slot = SaveGame::slot(&dir, "Slot 1 / Bad!");
        assert!(!slot.name.contains(' '));
        assert!(!slot.name.contains('/'));
        assert!(!slot.name.contains('!'));
        assert_eq!(slot.name, "Slot_1___Bad_");
    }

    /// The byte-level seam: `encode` produces exactly what `save` writes, and `decode`
    /// restores it into a fresh world without touching the filesystem.
    #[test]
    fn encode_then_decode_round_trips_without_the_filesystem() {
        let mut app = app();
        app.world_mut().spawn((
            Saveable,
            Health { hp: 12.5 },
            Transform {
                translation: Vec3::new(4.0, 5.0, 6.0),
                ..Default::default()
            },
        ));
        app.insert_resource(Score(7));

        let bytes = SaveGame::encode(app.world_mut(), &SaveSpec::default()).expect("encode");
        assert!(bytes.len() > 12);
        assert_eq!(&bytes[..4], SAVE_MAGIC, "same container as on disk");

        // Clear the world, then restore from the bytes alone.
        let to_clear: Vec<Entity> = app
            .world_mut()
            .query_filtered::<Entity, With<Health>>()
            .iter(app.world_mut())
            .collect();
        for entity in to_clear {
            app.world_mut().despawn(entity);
        }
        app.insert_resource(Score(0));

        let restored = SaveGame::decode(app.world_mut(), &bytes, &SaveSpec::default()).expect("decode");
        assert_eq!(restored.entity_count, 1);
        assert_eq!(restored.bytes_written, bytes.len());
        let world = app.world_mut();
        let mut q = world.query::<&Health>();
        assert_eq!(q.iter(world).next().expect("restored entity").hp, 12.5);
        assert_eq!(app.world().resource::<Score>().0, 7);
    }

    /// Damaged bytes are rejected by `decode` with the same corruption errors as `load`.
    #[test]
    fn decode_rejects_damaged_bytes() {
        let mut app = app();
        app.world_mut().spawn((Saveable, Health { hp: 1.0 }));
        let mut bytes = SaveGame::encode(app.world_mut(), &SaveSpec::default()).expect("encode");
        // Flip a payload byte: the CRC catches it.
        let mid = bytes.len() / 2;
        bytes[mid] = bytes[mid].wrapping_add(1);
        let err = SaveGame::decode(app.world_mut(), &bytes, &SaveSpec::default())
            .expect_err("CRC mismatch must fail");
        assert!(matches!(err, SaveError::Corrupt(_)), "{err}");

        // Bad magic is caught too.
        bytes[..4].copy_from_slice(b"XXXX");
        let err =
            SaveGame::decode(app.world_mut(), &bytes, &SaveSpec::default()).expect_err("bad magic must fail");
        assert!(matches!(err, SaveError::Corrupt(_)), "{err}");
    }

    #[test]
    fn newer_version_is_rejected_before_decode() {
        let dir = temp_dir("newer");
        let path = dir.join("Newer.save");
        // A save stamped with a future version and a payload that is not valid for the current
        // struct. The version check must reject it before the payload is decoded.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(SAVE_MAGIC);
        bytes.extend_from_slice(&999u32.to_le_bytes());
        let payload = b"not a valid v1 save payload";
        bytes.extend_from_slice(payload);
        bytes.extend_from_slice(&crc32(payload).to_le_bytes());
        std::fs::write(&path, &bytes).expect("write");

        let slot = SaveSlot {
            name: "Newer".into(),
            path: path.clone(),
            header: None,
        };
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        let err = SaveGame::load_raw(&slot, app.world_mut()).expect_err("newer version must be rejected");
        assert!(err.to_string().contains("newer"), "{err}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn migration_is_applied() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);

        let mut migrations = SaveMigrations::default();
        // Migrate from version 0 to 1.
        migrations.register(0, |_ctx, raw| {
            raw.game_version = "migrated".into();
        });
        app.insert_resource(migrations);

        let dir = temp_dir("migrate");
        let path = dir.join("Old.save");
        let raw = SaveFileRaw {
            version: 0,
            game_version: "old".into(),
            engine_version: "test".into(),
            timestamp_secs: 0.0,
            thumbnail: None,
            entities: vec![],
            resources: vec![],
            entity_ids: Vec::new(),
        };
        let mut bytes = Vec::new();
        bytes.extend_from_slice(SAVE_MAGIC);
        bytes.extend_from_slice(&0u32.to_le_bytes());
        let payload = postcard::to_allocvec(&raw).expect("serialize");
        let crc = crc32(&payload);
        bytes.extend_from_slice(&payload);
        bytes.extend_from_slice(&crc.to_le_bytes());
        std::fs::write(&path, &bytes).expect("write");

        let slot = SaveSlot {
            name: "Old".into(),
            path: path.clone(),
            header: None,
        };
        let loaded = SaveGame::load_raw(&slot, app.world_mut()).expect("load");
        assert_eq!(loaded.game_version, "migrated");

        std::fs::remove_file(&path).ok();
    }

    /// Host-side: one write on the `Suspended` transition, not on later frames of the same
    /// suspension, and a new write after `Resumed` arms the next edge.
    ///
    /// [`regia_platform::apply_transition`] is what the runner calls for the OS event, on
    /// every platform. Leaving `last_event` stuck at `Suspended` and ticking `App::update`
    /// (the old per-frame hook) must not write again.
    #[test]
    fn autosave_writes_once_on_the_suspended_edge() {
        let dir = temp_dir("autosave-edge");
        let mut app = app();
        // Headless `MinimalPlugins` has no platform runner. The resource and the message
        // are what `PlatformPlugin` installs; `apply_transition` is the same call the
        // runner makes when the OS suspends the process.
        app.init_resource::<regia_platform::AppLifecycle>()
            .add_message::<regia_platform::LifecycleEvent>();
        app.insert_resource(SaveDirectory(dir.clone()));
        app.insert_resource(Score(1));
        app.world_mut().spawn((Saveable, Health { hp: 3.0 }));

        app.update();
        let slot = SaveGame::slot(&SaveDirectory(dir.clone()), "Auto");
        assert!(!slot.path.exists(), "running frames do not autosave");

        regia_platform::apply_transition(&mut app, regia_platform::LifecycleEvent::Suspended);
        assert!(slot.path.is_file(), "the Suspended edge writes the Auto slot");
        let first = std::fs::read(&slot.path).expect("read first autosave");

        // The world has moved on, and a stuck `last_event` plus ordinary frames must not
        // overwrite the slot. A per-frame Update hook would.
        app.insert_resource(Score(7));
        {
            let mut lifecycle = app.world_mut().resource_mut::<regia_platform::AppLifecycle>();
            lifecycle.state = regia_platform::LifecycleState::Suspended;
            lifecycle.last_event = Some(regia_platform::LifecycleEvent::Suspended);
        }
        app.update();
        app.update();
        let stuck = std::fs::read(&slot.path).expect("read after stuck frames");
        assert_eq!(stuck, first, "suspended frames did not write Score 7");

        // A second Suspended transition without Resumed is the same edge.
        regia_platform::apply_transition(&mut app, regia_platform::LifecycleEvent::Suspended);
        let again = std::fs::read(&slot.path).expect("read after second Suspended");
        assert_eq!(again, first, "a repeated Suspended did not write again");

        regia_platform::apply_transition(&mut app, regia_platform::LifecycleEvent::Resumed);
        app.insert_resource(Score(7));
        regia_platform::apply_transition(&mut app, regia_platform::LifecycleEvent::Suspended);
        SaveGame::load(&slot, app.world_mut(), &SaveSpec::default()).expect("load second autosave");
        assert_eq!(
            app.world().resource::<Score>().0,
            7,
            "a new suspension after Resumed saves the world as it is then"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Threat model finding 18 (ADR-3285): the streamed header of a slot list agrees with
    /// `SaveHeader::from_bytes` on valid saves (with and without a thumbnail, a payload longer than
    /// the CRC chunk) and refuses what it refuses (bad magic, a flipped payload byte, a short file).
    #[test]
    fn the_streamed_header_matches_the_whole_file_header() {
        let dir = regia_test_utils::unique_temp_dir("save-header-stream");
        let saves = [
            (None, Vec::new()),
            (Some(vec![7u8; 300]), Vec::new()),
            // Entities past the 64 KiB CRC chunk and header prefix.
            (
                Some(vec![1u8; 10]),
                (0..400)
                    .map(|i| SavedEntity {
                        components: BTreeMap::from([(format!("C{i}"), vec![i as u8; 512])]),
                    })
                    .collect(),
            ),
        ];
        for (n, (thumbnail, entities)) in saves.into_iter().enumerate() {
            let raw = SaveFileRaw {
                version: SAVE_FORMAT_VERSION,
                game_version: format!("game-{n}"),
                engine_version: "engine".into(),
                timestamp_secs: 12.5 + n as f64,
                thumbnail,
                entities,
                resources: vec![],
                entity_ids: Vec::new(),
            };
            let bytes = frame_save_file(&raw).expect("frame");
            let path = dir.join(format!("s{n}.save"));
            std::fs::write(&path, &bytes).expect("write");
            let whole = SaveHeader::from_bytes(&bytes).expect("a valid save");
            let streamed = read_header_streaming(&path)
                .expect("readable")
                .expect("a valid save");
            assert_eq!(
                (streamed.version, &streamed.game_version, &streamed.engine_version),
                (whole.version, &whole.game_version, &whole.engine_version)
            );
            assert_eq!(streamed.timestamp_secs.to_bits(), whole.timestamp_secs.to_bits());
            assert_eq!(streamed.has_thumbnail, whole.has_thumbnail);

            let mut damaged = bytes.clone();
            let middle = damaged.len() / 2;
            damaged[middle] ^= 0x40;
            std::fs::write(&path, &damaged).expect("write");
            assert!(
                read_header_streaming(&path).expect("readable").is_none(),
                "a flipped byte"
            );
            let mut tagged = bytes.clone();
            tagged[0] ^= 0xff;
            std::fs::write(&path, &tagged).expect("write");
            assert!(
                read_header_streaming(&path).expect("readable").is_none(),
                "bad magic"
            );
            std::fs::write(&path, &bytes[..11]).expect("write");
            assert!(read_header_streaming(&path).expect("readable").is_none(), "short");
        }
        assert!(matches!(
            read_header_streaming(&dir.join("missing.save")),
            Err(SaveError::Io(e, _)) if e.kind() == std::io::ErrorKind::NotFound
        ));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn crc32_in_chunks_equals_crc32_whole() {
        let data: Vec<u8> = (0..10_000u32).map(|i| (i * 31 % 251) as u8).collect();
        let chunked = data.chunks(777).fold(crc32_begin(), crc32_update);
        assert_eq!(crc32_finish(chunked), crc32(&data));
        // The IEEE check value of "123456789".
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    /// A payload whose first bytes are not a decodable version varint (empty, or five
    /// continuation bytes) can never load. The refusal names the missing version, not the postcard
    /// decode failure it used to fall through to via `unwrap_or(file_version)` (w10-v051-smalls).
    #[test]
    fn an_undecodable_payload_version_is_refused_by_name() {
        let mut app = app();
        for payload in [Vec::new(), vec![0x80u8; 3], vec![0xFFu8; 6]] {
            for header_version in [0u32, 1, SAVE_FORMAT_VERSION] {
                let mut bytes = Vec::new();
                bytes.extend_from_slice(SAVE_MAGIC);
                bytes.extend_from_slice(&header_version.to_le_bytes());
                bytes.extend_from_slice(&payload);
                bytes.extend_from_slice(&crc32(&payload).to_le_bytes());
                let err = parse_save_file(&bytes, app.world_mut()).expect_err("must be refused");
                let SaveError::Deserialize(message) = &err else {
                    panic!("expected Deserialize, got {err:?}");
                };
                assert!(
                    message.contains("decodable version varint"),
                    "header {header_version}, payload {payload:?}: {message}"
                );
            }
        }
    }
}
