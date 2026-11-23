// SPDX-License-Identifier: MPL-2.0
//! The save flow (`docs/specs/91-character-save-templates.md` § 2.1): where saves are stored,
//! saving from ordinary systems, quicksave, opaque game data and slot info.
//!
//! [`SaveGame`] serializes the world and needs `&mut World`; this module is the layer a game
//! talks to:
//!
//! - **Store.** A [`SaveStore`] resource holds the [`SaveStorage`] that keeps save bytes (the
//!   [`SaveGame::encode`] container, unchanged). Without one, slots are files in the
//!   [`SaveDirectory`] ([`FileSaveStorage`], the same `<dir>/<slot>.save` path and atomic write as
//!   [`SaveGame::save`]). `regia_steam` implements the trait for Steam Remote Storage, so cloud
//!   saves are one `insert_resource`.
//! - **Requests.** Any system writes a [`SaveRequest`] message; one exclusive system drains them in
//!   `Last`, in arrival order, and answers each with one [`SaveOutcome`].
//! - **Quicksave.** [`QuickSaveConfig`] names the quick slot and two input actions (`QuickSave`,
//!   `QuickLoad`); pressing them sends the requests.
//! - **Game data.** [`SaveBlobs`] (opaque bytes by key) and [`SaveInfo`] (display name, level, play
//!   time, save count) are ordinary saveable resources inside save format version 1, so old files
//!   load unchanged (spec 91 § 3).
//!
//! Add [`SaveFlowPlugin`] after [`SavePlugin`](crate::save_game::SavePlugin).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use regia_app::prelude::*;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::save_game::{
    LastSaveLoad, RegisterSaveableExt, SaveCopy, SaveDirectory, SaveError, SaveGame, SaveHeader, SaveResult,
    SaveSpec, SaveableResource, delete_slot_files, previous_path, read_header_streaming, read_save_bytes,
    write_atomic_keep_previous,
};

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

/// Where save-file bytes are kept. The bytes are exactly [`SaveGame::encode`]'s container (magic,
/// version, postcard payload, CRC-32 footer); a storage decides only where they live.
///
/// Slot names reaching a storage are already sanitized ([`sanitize_slot_name`]) and never empty.
pub trait SaveStorage: Send + Sync + 'static {
    /// A short name for the storage (`"file"`, `"steam_remote"`, ...), reported by the agent's
    /// `save.status`.
    fn kind(&self) -> &'static str;

    /// Writes `bytes` to `slot`, replacing it atomically (a failed write leaves the old slot).
    ///
    /// # Errors
    /// Any IO or backend failure.
    fn write(&self, slot: &str, bytes: &[u8]) -> Result<(), SaveError>;

    /// Reads `slot`; `Ok(None)` when the slot does not exist.
    ///
    /// # Errors
    /// Any IO or backend failure other than "not found".
    fn read(&self, slot: &str) -> Result<Option<Vec<u8>>, SaveError>;

    /// Deletes `slot`. `Ok(false)` when it did not exist.
    ///
    /// # Errors
    /// Any IO or backend failure.
    fn delete(&self, slot: &str) -> Result<bool, SaveError>;

    /// Every slot name in this storage, sorted.
    ///
    /// # Errors
    /// Any IO or backend failure.
    fn list(&self) -> Result<Vec<String>, SaveError>;

    /// The header of `slot` for a slot list; `Ok(None)` when the slot is missing or its bytes are
    /// not a valid save. The default reads the slot whole ([`SaveStorage::read`]); a storage that
    /// can read a prefix overrides it ([`FileSaveStorage`] holds one fixed chunk in memory
    /// whatever the file's size).
    ///
    /// # Errors
    /// Any IO or backend failure, or a slot over the storage's size cap.
    fn read_header(&self, slot: &str) -> Result<Option<SaveHeader>, SaveError> {
        Ok(self.read(slot)?.as_deref().and_then(header_from_bytes))
    }

    /// Reads the previous good generation of `slot`, which a load uses when the current copy is
    /// damaged or missing (ADR-3102). `Ok(None)` when there is none; the default keeps none
    /// (a cloud store keeps no `.prev`: nothing extra is uploaded).
    ///
    /// # Errors
    /// Any IO or backend failure other than "not found".
    fn read_previous(&self, slot: &str) -> Result<Option<Vec<u8>>, SaveError> {
        let _ = slot;
        Ok(None)
    }

    /// The header of the previous generation, for a slot list. `Ok(None)` when there is none
    /// or its bytes are not a valid save. The default reads the previous copy whole
    /// ([`SaveStorage::read_previous`]); a file storage overrides it and refuses a copy over
    /// the size cap by its length, without reading the bytes.
    ///
    /// # Errors
    /// Any IO or backend failure, or a copy over the storage's size cap.
    fn read_previous_header(&self, slot: &str) -> Result<Option<SaveHeader>, SaveError> {
        Ok(self.read_previous(slot)?.as_deref().and_then(header_from_bytes))
    }

    /// Whether `slot` has a previous generation (`save.list_slots` reports it). The default
    /// keeps none.
    fn has_previous(&self, slot: &str) -> bool {
        let _ = slot;
        false
    }
}

/// The default storage: one file per slot, `<dir>/<slot>.save`, written atomically (the paths
/// [`SaveGame::slot`] and [`SaveGame::list_slots`] use), with one previous good generation in
/// `<dir>/<slot>.prev` (ADR-3102). A delete removes both.
#[derive(Debug, Clone)]
pub struct FileSaveStorage {
    /// The directory holding the slot files.
    pub dir: PathBuf,
}

impl FileSaveStorage {
    /// A file storage in `dir`.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    fn path(&self, slot: &str) -> PathBuf {
        self.dir.join(format!("{slot}.save"))
    }
}

impl SaveStorage for FileSaveStorage {
    fn kind(&self) -> &'static str {
        "file"
    }

    fn write(&self, slot: &str, bytes: &[u8]) -> Result<(), SaveError> {
        write_atomic_keep_previous(&self.path(slot), bytes).map(drop)
    }

    /// Reads the slot file, refusing one over
    /// [`MAX_SAVE_FILE_BYTES`](crate::save_game::MAX_SAVE_FILE_BYTES) by its size before reading it
    /// (`SaveError::Corrupt`), the cap [`SaveGame::load`] applies (threat model finding 18,
    /// ADR-3285).
    fn read(&self, slot: &str) -> Result<Option<Vec<u8>>, SaveError> {
        match read_save_bytes(&self.path(slot)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(SaveError::Io(err, _)) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// Reads only the header: a file over the cap is refused by its size, the CRC-32 is checked
    /// in fixed chunks and the header is decoded from the payload's first bytes, so listing a big
    /// `.save` in a shared folder costs no more memory than a small one.
    fn read_header(&self, slot: &str) -> Result<Option<SaveHeader>, SaveError> {
        match read_header_streaming(&self.path(slot)) {
            Err(SaveError::Io(err, _)) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            other => other,
        }
    }

    fn delete(&self, slot: &str) -> Result<bool, SaveError> {
        delete_slot_files(&self.path(slot))
    }

    fn read_previous(&self, slot: &str) -> Result<Option<Vec<u8>>, SaveError> {
        read_optional(&previous_path(&self.path(slot)))
    }

    fn read_previous_header(&self, slot: &str) -> Result<Option<SaveHeader>, SaveError> {
        match read_header_streaming(&previous_path(&self.path(slot))) {
            Err(SaveError::Io(err, _)) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            other => other,
        }
    }

    fn has_previous(&self, slot: &str) -> bool {
        previous_path(&self.path(slot)).is_file()
    }

    fn list(&self) -> Result<Vec<String>, SaveError> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(SaveError::Io(err, self.dir.clone())),
        };
        // A slot with only a previous copy (a crash between the two renames of a save) is listed
        // too: its load reads that copy.
        let mut names: Vec<String> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension().is_some_and(|ext| ext == "save")
                    || (path
                        .extension()
                        .is_some_and(|ext| ext == crate::save_game::SAVE_PREVIOUS_EXTENSION)
                        && !path.with_extension("save").exists())
            })
            .filter_map(|path| path.file_stem().map(|stem| stem.to_string_lossy().into_owned()))
            .collect();
        names.sort();
        names.dedup();
        Ok(names)
    }
}

/// Reads `path`, refusing one over the save size cap by its length before any byte is read.
/// "not found" is `Ok(None)`.
fn read_optional(path: &std::path::Path) -> Result<Option<Vec<u8>>, SaveError> {
    match read_save_bytes(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(SaveError::Io(err, _)) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

/// The storage the save flow and the agent's `save.*` methods use. Absent: files in the
/// [`SaveDirectory`] ([`FileSaveStorage`]).
#[derive(Resource, Clone)]
pub struct SaveStore(pub Arc<dyn SaveStorage>);

impl SaveStore {
    /// Wraps a storage.
    pub fn new(storage: impl SaveStorage) -> Self {
        Self(Arc::new(storage))
    }
}

impl std::fmt::Debug for SaveStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("SaveStore").field(&self.0.kind()).finish()
    }
}

/// The storage in effect: the [`SaveStore`] if one is installed, else files in the
/// [`SaveDirectory`] (its default when that is missing too).
pub fn active_storage(world: &World) -> Arc<dyn SaveStorage> {
    if let Some(store) = world.get_resource::<SaveStore>() {
        return store.0.clone();
    }
    let dir = world.get_resource::<SaveDirectory>().cloned().unwrap_or_default();
    Arc::new(FileSaveStorage::new(dir.0))
}

/// The slot-name rule every storage sees: ASCII letters, digits, `-` and `_` are kept, anything
/// else becomes `_` (the rule [`SaveGame::slot`] applies to file names). A name that is a Windows
/// device name (`con`, `NUL`, `com1`, ...) gets a trailing `_`, on every platform, so a slot saves
/// the same everywhere (Windows cannot create `con.save`).
pub fn sanitize_slot_name(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if regia_platform::is_reserved_device_name(&out) {
        out.push('_');
    }
    out
}

/// The sanitized slot name, or an `InvalidInput` error for an empty one.
fn checked_slot(name: &str) -> Result<String, SaveError> {
    let slot = sanitize_slot_name(name);
    if slot.is_empty() {
        return Err(SaveError::Io(
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "empty save slot name"),
            PathBuf::new(),
        ));
    }
    Ok(slot)
}

// ---------------------------------------------------------------------------
// Store operations
// ---------------------------------------------------------------------------

/// Serializes the world ([`SaveGame::encode`]) and writes it to `slot` in the active storage.
/// [`SaveInfo::save_count`] is bumped first, so the file holds the count it was written as; a
/// save that fails to encode or write puts the count back.
///
/// # Errors
/// An empty slot name, a serialization failure or a storage failure.
pub fn save_to_store(world: &mut World, slot: &str) -> Result<SaveResult, SaveError> {
    let slot = checked_slot(slot)?;
    let previous_count = world.get_resource_mut::<SaveInfo>().map(|mut info| {
        let previous = info.save_count;
        info.save_count = previous.saturating_add(1);
        previous
    });
    let written = SaveGame::encode_counted(world, &SaveSpec::default())
        .and_then(|(bytes, result)| active_storage(world).write(&slot, &bytes).map(|()| result));
    let result = match written {
        Ok(result) => result,
        Err(e) => {
            if let (Some(previous), Some(mut info)) = (previous_count, world.get_resource_mut::<SaveInfo>()) {
                info.save_count = previous;
            }
            return Err(e);
        }
    };
    tracing::info!(
        target: "regia_framework",
        "saved {} entities and {} resources to slot {slot}",
        result.entity_count, result.resource_count
    );
    Ok(result)
}

/// Reads `slot` from the active storage and applies it ([`SaveGame::decode`]: migrations run,
/// saveable entities are replaced, registered resources restored).
///
/// When the current copy is damaged (`SaveError::Corrupt`) or missing and the storage keeps a
/// previous generation ([`SaveStorage::read_previous`]), that copy loads instead, one warn line
/// says so, and [`SaveResult::copy`] is [`SaveCopy::Previous`] (ADR-3102). The slot and copy are
/// recorded in [`LastSaveLoad`].
///
/// # Errors
/// An empty slot name, a missing slot (`SaveError::Io` of kind `NotFound`), a storage failure or
/// a corrupt file with no good previous copy.
pub fn load_from_store(world: &mut World, slot: &str) -> Result<SaveResult, SaveError> {
    let slot = checked_slot(slot)?;
    let storage = active_storage(world);
    let current = storage.read(&slot);
    let current_label = format!("{slot} ({})", storage.kind());
    let previous_label = format!("{slot} previous ({})", storage.kind());
    let parsed = crate::save_game::parse_with_fallback(
        world,
        &slot,
        current,
        &current_label,
        &mut || storage.read_previous(&slot),
        &previous_label,
    )?;
    let Some((raw, bytes_read, copy)) = parsed else {
        return Err(SaveError::Io(
            std::io::Error::new(std::io::ErrorKind::NotFound, format!("no save slot {slot}")),
            PathBuf::from(&slot),
        ));
    };
    let result = SaveGame::apply_parsed(world, &raw, bytes_read, copy, &SaveSpec::default())?;
    world.insert_resource(LastSaveLoad {
        slot: slot.clone(),
        copy,
    });
    tracing::info!(
        target: "regia_framework",
        "loaded {} entities and {} resources from slot {slot} ({} copy)",
        result.entity_count, result.resource_count, copy.as_str()
    );
    Ok(result)
}

/// Deletes `slot` from the active storage: on purpose, so the file storage removes its previous
/// generation too. `Ok(false)` when it did not exist.
///
/// # Errors
/// An empty slot name or a storage failure.
pub fn delete_from_store(world: &World, slot: &str) -> Result<bool, SaveError> {
    let slot = checked_slot(slot)?;
    active_storage(world).delete(&slot)
}

/// One slot of [`list_store`]: its name and the header parsed from its bytes (`None` when the
/// bytes fail validation: treat the slot as damaged).
#[derive(Debug, Clone)]
pub struct SlotListing {
    /// The slot name.
    pub name: String,
    /// The header, when the file is valid (the previous copy's header when only that one is).
    pub header: Option<SaveHeader>,
    /// Whether the slot has a previous good generation ([`SaveStorage::has_previous`], ADR-3102).
    pub has_previous: bool,
}

/// Every slot of the active storage with its header ([`SaveStorage::read_header`]), sorted by
/// name.
///
/// # Errors
/// A storage failure while listing. A slot that fails to read is listed with no header.
pub fn list_store(world: &World) -> Result<Vec<SlotListing>, SaveError> {
    let storage = active_storage(world);
    let mut slots = Vec::new();
    for name in storage.list()? {
        let has_previous = storage.has_previous(&name);
        let header = storage.read_header(&name).ok().flatten().or_else(|| {
            if !has_previous {
                return None;
            }
            storage.read_previous_header(&name).ok().flatten()
        });
        slots.push(SlotListing {
            name,
            header,
            has_previous,
        });
    }
    Ok(slots)
}

/// The header of save-file bytes (the [`SaveGame::encode`] container), or `None` when the magic,
/// length or CRC check fails.
pub fn header_from_bytes(bytes: &[u8]) -> Option<SaveHeader> {
    SaveHeader::from_bytes(bytes)
}

// ---------------------------------------------------------------------------
// Game data
// ---------------------------------------------------------------------------

/// Opaque game data stored in every save, by key: state a game serializes itself (a quest log in
/// its own format, script state it chooses to keep). Keys sort, so the saved bytes are stable.
#[derive(Resource, Reflect, Default, Clone, Debug, PartialEq)]
#[reflect(Resource, Default)]
pub struct SaveBlobs {
    /// The blobs by key.
    pub entries: BTreeMap<String, Vec<u8>>,
}

impl SaveableResource for SaveBlobs {}

impl SaveBlobs {
    /// Stores `bytes` under `key`, replacing an earlier value.
    pub fn set(&mut self, key: impl Into<String>, bytes: Vec<u8>) {
        self.entries.insert(key.into(), bytes);
    }

    /// The bytes under `key`.
    pub fn get(&self, key: &str) -> Option<&[u8]> {
        self.entries.get(key).map(Vec::as_slice)
    }

    /// Removes and returns the bytes under `key`.
    pub fn remove(&mut self, key: &str) -> Option<Vec<u8>> {
        self.entries.remove(key)
    }

    /// The keys, sorted.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }

    /// Encodes `value` with postcard and stores it under `key`.
    ///
    /// # Errors
    /// The value fails to serialize.
    pub fn set_value<T: Serialize>(&mut self, key: impl Into<String>, value: &T) -> Result<(), SaveError> {
        let bytes = postcard::to_allocvec(value).map_err(|e| SaveError::Serialize(e.to_string()))?;
        self.set(key, bytes);
        Ok(())
    }

    /// Decodes the value under `key` with postcard; `None` when the key is absent.
    pub fn get_value<T: DeserializeOwned>(&self, key: &str) -> Option<Result<T, SaveError>> {
        self.get(key)
            .map(|bytes| postcard::from_bytes(bytes).map_err(|e| SaveError::Deserialize(e.to_string())))
    }
}

/// What a load menu shows for a slot; saved in every save.
#[derive(Resource, Reflect, Default, Clone, Debug, PartialEq)]
#[reflect(Resource, Default)]
pub struct SaveInfo {
    /// The name the player or game gave the save.
    pub display_name: String,
    /// The level (or chapter) the save was made in.
    pub level: String,
    /// Gameplay seconds played (virtual time: pauses do not count).
    pub play_time_secs: f64,
    /// How many saves this playthrough has written (bumped by [`save_to_store`]).
    pub save_count: u32,
}

impl SaveableResource for SaveInfo {}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

/// A save operation asked for by any system; drained once per frame in `Last`.
#[derive(Message, Clone, Debug, PartialEq, Eq)]
pub enum SaveRequest {
    /// Save the world to a slot.
    Save {
        /// The slot.
        slot: String,
    },
    /// Load a slot into the world.
    Load {
        /// The slot.
        slot: String,
    },
    /// Delete a slot.
    Delete {
        /// The slot.
        slot: String,
    },
    /// Save to the quick slot ([`QuickSaveConfig::slot`]).
    QuickSave,
    /// Load the quick slot.
    QuickLoad,
}

/// Which operation a [`SaveOutcome`] answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaveOp {
    /// A save (plain or quick).
    Save,
    /// A load (plain or quick).
    Load,
    /// A delete.
    Delete,
}

/// The answer to one [`SaveRequest`].
#[derive(Message, Clone, Debug)]
pub struct SaveOutcome {
    /// The operation.
    pub op: SaveOp,
    /// The slot it touched (sanitized).
    pub slot: String,
    /// Whether it came from a quick request.
    pub quick: bool,
    /// Counts and size on success (a delete reports zero counts, and `bytes_written` 1 when a
    /// slot was removed, 0 when it was missing); the error text on failure.
    pub result: Result<SaveResult, String>,
}

/// The quick slot and the input actions that save to and load from it.
#[derive(Resource, Clone, Debug, PartialEq, Eq)]
pub struct QuickSaveConfig {
    /// The quick slot name (default `"Quick"`).
    pub slot: String,
    /// The action that quicksaves (default `"QuickSave"`).
    pub save_action: String,
    /// The action that quickloads (default `"QuickLoad"`).
    pub load_action: String,
}

impl Default for QuickSaveConfig {
    fn default() -> Self {
        Self {
            slot: "Quick".into(),
            save_action: "QuickSave".into(),
            load_action: "QuickLoad".into(),
        }
    }
}

/// Runs one request against the world and builds its outcome.
pub fn run_save_request(world: &mut World, request: &SaveRequest) -> SaveOutcome {
    let quick_slot = || {
        world
            .get_resource::<QuickSaveConfig>()
            .map_or_else(|| QuickSaveConfig::default().slot, |config| config.slot.clone())
    };
    let (op, slot, quick) = match request {
        SaveRequest::Save { slot } => (SaveOp::Save, slot.clone(), false),
        SaveRequest::Load { slot } => (SaveOp::Load, slot.clone(), false),
        SaveRequest::Delete { slot } => (SaveOp::Delete, slot.clone(), false),
        SaveRequest::QuickSave => (SaveOp::Save, quick_slot(), true),
        SaveRequest::QuickLoad => (SaveOp::Load, quick_slot(), true),
    };
    let result = match op {
        SaveOp::Save => save_to_store(world, &slot),
        SaveOp::Load => load_from_store(world, &slot),
        SaveOp::Delete => delete_from_store(world, &slot).map(|deleted| SaveResult {
            entity_count: 0,
            resource_count: 0,
            bytes_written: usize::from(deleted),
            copy: SaveCopy::Current,
        }),
    };
    if let Err(err) = &result {
        tracing::warn!(target: "regia_framework", "save request {op:?} on slot {slot} failed: {err}");
    }
    SaveOutcome {
        op,
        slot: sanitize_slot_name(&slot),
        quick,
        result: result.map_err(|err| err.to_string()),
    }
}

/// `Last`: drains every [`SaveRequest`] of the frame in arrival order and answers each with a
/// [`SaveOutcome`].
#[tracing::instrument(skip_all, target = "regia_framework", name = "save_flow_requests")]
fn process_save_requests(world: &mut World) {
    let requests: Vec<SaveRequest> = match world.get_resource_mut::<Messages<SaveRequest>>() {
        Some(mut messages) => messages.drain().collect(),
        None => return,
    };
    for request in requests {
        let outcome = run_save_request(world, &request);
        world.write_message(outcome);
    }
}

/// `Update`: the quick actions become requests.
fn quick_save_input(
    actions: Option<Res<regia_input::Actions>>,
    config: Res<QuickSaveConfig>,
    mut requests: MessageWriter<SaveRequest>,
) {
    let Some(actions) = actions else {
        return;
    };
    if actions.just_pressed_this_frame(&config.save_action) {
        requests.write(SaveRequest::QuickSave);
    }
    if actions.just_pressed_this_frame(&config.load_action) {
        requests.write(SaveRequest::QuickLoad);
    }
}

/// `Update`: gameplay time accrues into [`SaveInfo::play_time_secs`].
fn tick_play_time(time: Res<Time>, info: Option<ResMut<SaveInfo>>) {
    if let Some(mut info) = info {
        let delta = f64::from(time.delta_secs());
        if delta > 0.0 {
            info.play_time_secs += delta;
        }
    }
}

/// Installs the save flow: [`SaveBlobs`] and [`SaveInfo`] (saveable), the request and outcome
/// messages, [`QuickSaveConfig`] (kept if the game inserted its own), the quick actions and play
/// time in `Update`, and the request drain in `Last`. Add after [`SavePlugin`](crate::save_game::SavePlugin).
pub struct SaveFlowPlugin;

impl Plugin for SaveFlowPlugin {
    fn build(&self, app: &mut App) {
        app.register_saveable_resource::<SaveBlobs>()
            .register_saveable_resource::<SaveInfo>()
            .init_resource::<SaveBlobs>()
            .init_resource::<SaveInfo>()
            .init_resource::<QuickSaveConfig>()
            .add_message::<SaveRequest>()
            .add_message::<SaveOutcome>()
            .add_systems(Update, (quick_save_input, tick_play_time))
            .add_systems(Last, process_save_requests);
    }
}

#[cfg(test)]
mod tests;
