// SPDX-License-Identifier: MPL-2.0
//! A dedicated server that keeps its world across restarts (persistent co-op and survival
//! servers; ADR-3354, `book/agents/net-server-persistence.md`), feature `net`.
//!
//! [`ServerPersistencePlugin`] saves the world through the save framework
//! ([`SaveGame::encode`](crate::save_game::SaveGame::encode): [`Saveable`](crate::save_game::Saveable)
//! entities, saveable resources and save sections) when the `server.toml` `[persistence]` section
//! is on (`regia_net::server_host::PersistenceSettings`):
//!
//! - **When.** Every `autosave_ticks` host ticks (the host's tick count, never the wall clock, so
//!   the number of autosaves is `ticks / autosave_ticks`), at the end of a tick in which a
//!   player left, before a level change (`save_on_level_change`), after the drain of a graceful
//!   shutdown (`save_on_shutdown`), and on `save` / `server.save_now`. A crash or a killed process
//!   saves nothing: the world comes back as of the last save.
//! - **Where.** `<server dir>/<dir>/<level folder>/world-<sequence>.save`, one folder per level
//!   (the level path made file-name safe plus a hash). The sequence counts up across every
//!   level's folder. Each write is atomic (a temp file of its own, fsync, rename, directory fsync:
//!   [`write_atomic`]), then the oldest saves beyond `keep` in that folder are removed.
//! - **Restore.** When the host starts ([`HostEvent::Started`], after the level loaded and before
//!   any session update lets a client in) and after each level change, the newest save of that
//!   level's folder that decodes is applied. A save that fails (truncated, torn, bad checksum,
//!   unreadable) is renamed `*.save.corrupt` (kept for the operator, never removed by the
//!   rotation) with warning `persistence_save_corrupt`, and the next older save is tried. When the
//!   folder holds saves and none decodes, the start fails (exit code 4,
//!   `persistence_no_good_save`) and the files stay as they are: a restart never silently starts
//!   a fresh world over a world it could not read.
//! - **Players.** Components a game registers with
//!   [`RegisterPlayerPersistentExt::register_player_persistent`] are kept per player, keyed by the
//!   session's stable identity: the authenticator's platform id (`steam:<id>`, spec 77 § 4), or
//!   with `player_identity = "platform_or_name"` the player name when there is none. They are read
//!   from the player's `NetPlayer` entity and from its pawn (the `GameRules` spawn,
//!   [`NetRulesJoined`]) when the player leaves and at every save, and written back once when that
//!   player joins again. The records travel inside the world save as the section
//!   [`PLAYER_SECTION_KEY`] (format `server-players` v1, `docs/formats/server-players.md`), so a
//!   save and its players are one atomic file. A record carries the sequence of the save it went
//!   into: at start the records of every level's newest good save are merged and the newest record
//!   per player wins.
//! - **Operator.** Console commands `save`, `persistence` and `restore <slot> confirm`; agent
//!   methods `server.save_now`, `server.persistence_status` and `server.restore` (regia_agent).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regia_app::prelude::*;
use regia_ecs::reflect::{ReflectComponent, TypeRegistry};
use regia_ecs::serialize::ReflectCodec;
use regia_net::model::NetPlayerId;
use regia_net::server_host::{HostEvent, HostGameHooks, PersistenceSettings, PlayerIdentityMode};
use regia_net::session::{NetPlayerHooks, NetSession};
use serde::{Deserialize, Serialize};

use crate::net_session_bridge::{NetRulesJoined, rules_player_joined};
use crate::save_game::{
    MAX_SAVE_FILE_BYTES, SaveError, SaveGame, SaveSections, SaveSlot, SaveSpec, write_atomic,
};

/// The save-section key of the per-player records (format `server-players`).
pub const PLAYER_SECTION_KEY: &str = "regia_framework::server_persistence::players";

/// The `format_version` of the `server-players` section this build writes and reads.
pub const PLAYER_SECTION_FORMAT_VERSION: u32 = 1;

/// The file-name prefix of a world save.
const SAVE_PREFIX: &str = "world-";
/// The suffix of a world save.
const SAVE_SUFFIX: &str = ".save";
/// The suffix a save gets when it fails to restore.
const CORRUPT_SUFFIX: &str = ".corrupt";
/// Warnings kept for the status.
const MAX_WARNINGS: usize = 32;
/// Component types per player record (bounds a crafted section).
const MAX_RECORD_COMPONENTS: usize = 256;

/// Why a save was taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SaveReason {
    /// Every `autosave_ticks` host ticks.
    Autosave,
    /// After the drain of a graceful shutdown.
    Shutdown,
    /// Before a level change.
    LevelChange,
    /// A player left in this tick.
    PlayerLeft,
    /// `save` / `server.save_now`.
    Manual,
}

impl SaveReason {
    /// The lowercase name (status, logs).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Autosave => "autosave",
            Self::Shutdown => "shutdown",
            Self::LevelChange => "level_change",
            Self::PlayerLeft => "player_left",
            Self::Manual => "manual",
        }
    }
}

/// A persistence failure: a stable lowercase code (agent error code) and a message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, thiserror::Error)]
#[error("{code}: {message}")]
pub struct PersistError {
    /// `persistence_disabled`, `persistence_write_failed`, `persistence_save_corrupt`,
    /// `persistence_slot_unknown`, `persistence_no_good_save` or `persistence_encode_failed`.
    pub code: &'static str,
    /// What happened.
    pub message: String,
}

impl PersistError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// One warning the status reports (newest last).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PersistWarning {
    /// `persistence_save_corrupt`, `persistence_player_section_unreadable`,
    /// `persistence_no_good_save`, `persistence_write_failed` or `persistence_rotation_failed`.
    pub code: &'static str,
    /// What happened (the file names it).
    pub message: String,
}

/// What a save wrote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SaveReport {
    /// Why.
    pub reason: SaveReason,
    /// The save's sequence number (its slot).
    pub sequence: u64,
    /// The level.
    pub level: String,
    /// The host tick it was taken at.
    pub tick: u64,
    /// The file.
    pub path: PathBuf,
    /// File size.
    pub bytes: usize,
    /// Saved entities.
    pub entities: usize,
    /// Player records in it.
    pub players: usize,
    /// Old saves the rotation removed.
    pub removed: usize,
}

/// What a restore applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RestoreReport {
    /// The level.
    pub level: String,
    /// The save applied (`None`: the folder had no save, the level starts fresh).
    pub sequence: Option<u64>,
    /// Its file.
    pub path: Option<PathBuf>,
    /// The host tick the save was taken at.
    pub saved_tick: Option<u64>,
    /// Restored entities.
    pub entities: usize,
    /// Newer saves that failed and were set aside (`*.save.corrupt`).
    pub fell_back: Vec<PathBuf>,
    /// Player records known after the restore.
    pub players: usize,
}

/// Save counts by reason.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct SaveCounts {
    /// Autosaves.
    pub autosave: u64,
    /// Shutdown saves.
    pub shutdown: u64,
    /// Level-change saves.
    pub level_change: u64,
    /// Saves after a player left.
    pub player_left: u64,
    /// Manual saves.
    pub manual: u64,
    /// Failed saves (any reason).
    pub failed: u64,
}

impl SaveCounts {
    fn bump(&mut self, reason: SaveReason) {
        match reason {
            SaveReason::Autosave => self.autosave += 1,
            SaveReason::Shutdown => self.shutdown += 1,
            SaveReason::LevelChange => self.level_change += 1,
            SaveReason::PlayerLeft => self.player_left += 1,
            SaveReason::Manual => self.manual += 1,
        }
    }
}

/// One player's kept state (the `server-players` section).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayerRecord {
    /// The stable identity (`steam:<id>`, `name:<name>`).
    pub identity: String,
    /// The player's name when it was captured.
    pub name: String,
    /// The sequence of the save it went into (newer wins on a merge).
    pub stamp: u64,
    /// Component short type path → typed postcard bytes, from the `NetPlayer` entity.
    pub player: BTreeMap<String, Vec<u8>>,
    /// The same from the player's pawn.
    pub pawn: BTreeMap<String, Vec<u8>>,
}

/// The `server-players` section payload (postcard; `format_version` first).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayerSection {
    /// [`PLAYER_SECTION_FORMAT_VERSION`].
    pub format_version: u32,
    /// The level the save was taken on.
    pub level: String,
    /// The host tick.
    pub tick: u64,
    /// The save's sequence.
    pub sequence: u64,
    /// Why it was taken.
    pub reason: SaveReason,
    /// The records, sorted by identity.
    pub players: Vec<PlayerRecord>,
}

impl PlayerSection {
    /// Encodes the section.
    pub fn to_bytes(&self) -> Result<Vec<u8>, SaveError> {
        postcard::to_stdvec(self).map_err(|e| SaveError::Serialize(format!("server-players: {e}")))
    }

    /// Decodes a section: a newer `format_version` is refused before the rest is read; records
    /// with more than 256 components and duplicate identities are refused.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, SaveError> {
        let (version, _) = postcard::take_from_bytes::<u32>(bytes)
            .map_err(|e| SaveError::Deserialize(format!("server-players: {e}")))?;
        if version != PLAYER_SECTION_FORMAT_VERSION {
            return Err(SaveError::Deserialize(format!(
                "server-players: format_version {version} (this build reads {PLAYER_SECTION_FORMAT_VERSION})"
            )));
        }
        let section: Self = postcard::from_bytes(bytes)
            .map_err(|e| SaveError::Deserialize(format!("server-players: {e}")))?;
        let mut seen = std::collections::BTreeSet::new();
        for r in &section.players {
            if r.player.len() + r.pawn.len() > MAX_RECORD_COMPONENTS {
                return Err(SaveError::Deserialize(format!(
                    "server-players: record `{}` has more than {MAX_RECORD_COMPONENTS} components",
                    r.identity
                )));
            }
            if !seen.insert(r.identity.as_str()) {
                return Err(SaveError::Deserialize(format!(
                    "server-players: identity `{}` twice",
                    r.identity
                )));
            }
        }
        Ok(section)
    }
}

/// The serde field-list fingerprint of the `server-players` section (hashed by
/// `cargo xtask check-formats`): the section, its records and the save reasons.
pub fn server_players_schema_fingerprint() -> String {
    let mut reasons = regia_core::schema::serde_variant_names::<SaveReason>();
    reasons.extend(regia_core::schema::serde_variant_shapes::<SaveReason>());
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<PlayerSection>(),
        regia_core::schema::serde_field_names::<PlayerRecord>(),
        reasons,
    ])
}

/// `server-players` is at its first version: no migration steps (a save without the section holds
/// no player records, which is how saves from before ADR-3354 read).
pub fn server_players_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// Component types kept per player ([`RegisterPlayerPersistentExt`]).
#[derive(Resource, Debug, Clone, Default)]
pub struct PlayerPersistentComponents {
    types: Vec<(std::any::TypeId, &'static str)>,
}

impl PlayerPersistentComponents {
    /// The registered types' paths.
    pub fn type_paths(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.types.iter().map(|(_, p)| *p)
    }
}

/// Registers a component type as kept per player (ADR-3354).
pub trait RegisterPlayerPersistentExt {
    /// `T` on a player's `NetPlayer` entity or pawn is saved when the player leaves and at every
    /// save, and written back when the player joins again. Registers `T` for reflection.
    fn register_player_persistent<T>(&mut self) -> &mut Self
    where
        T: Component + Reflect + TypePath + GetTypeRegistration;
}

impl RegisterPlayerPersistentExt for App {
    fn register_player_persistent<T>(&mut self) -> &mut Self
    where
        T: Component + Reflect + TypePath + GetTypeRegistration,
    {
        self.register_type::<T>();
        let mut list = self
            .world_mut()
            .get_resource_or_insert_with(PlayerPersistentComponents::default);
        let id = std::any::TypeId::of::<T>();
        if !list.types.iter().any(|(t, _)| *t == id) {
            list.types.push((id, T::type_path()));
        }
        self
    }
}

/// On a player's `NetPlayer` entity once its kept state was looked up (a join after a travel or a
/// resume does not restore again).
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct PersistentPlayer {
    /// The stable identity.
    pub identity: String,
    /// The player's name at join (the session may have forgotten the peer by the time it leaves).
    pub name: String,
    /// The pawn named at join (the `GameRules` spawn), if any.
    pub pawn: Option<Entity>,
}

/// How a section read treats the records already in memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum MergeMode {
    /// Keep the newer record per identity (start, level change).
    #[default]
    KeepNewer,
    /// The save's records replace the store (an operator `restore`).
    Replace,
}

/// The state of the server's world persistence (one per server app).
#[derive(Resource, Debug, Clone, Default)]
pub struct ServerPersistence {
    settings: PersistenceSettings,
    active: bool,
    root: PathBuf,
    level: String,
    next_sequence: u64,
    tick: u64,
    pending_player_save: bool,
    store: BTreeMap<String, PlayerRecord>,
    counts: SaveCounts,
    last_save: Option<SaveReport>,
    last_restore: Option<RestoreReport>,
    warnings: Vec<PersistWarning>,
    players_restored: u64,
    players_unidentified: u64,
    // Set around one encode / decode.
    writing: Option<(u64, SaveReason)>,
    merge: MergeMode,
    read_section: Option<PlayerSection>,
}

/// One save on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SaveEntry {
    /// The sequence (the slot `restore` takes).
    pub sequence: u64,
    /// The file.
    pub path: PathBuf,
    /// File size.
    pub bytes: u64,
}

/// What `server.persistence_status` reports.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PersistenceStatus {
    /// `[persistence] enabled` and the host started.
    pub enabled: bool,
    /// The settings.
    pub settings: PersistenceSettings,
    /// The saves folder (absolute).
    pub root: PathBuf,
    /// The current level.
    pub level: String,
    /// The last host tick seen.
    pub tick: u64,
    /// The host tick of the next autosave (`None`: autosave off).
    pub next_autosave_tick: Option<u64>,
    /// The sequence the next save gets.
    pub next_sequence: u64,
    /// Saves by reason.
    pub counts: SaveCounts,
    /// The last save.
    pub last_save: Option<SaveReport>,
    /// The last restore.
    pub last_restore: Option<RestoreReport>,
    /// The current level's saves, newest first.
    pub saves: Vec<SaveEntry>,
    /// Saves set aside as corrupt in the current level's folder.
    pub corrupt: Vec<PathBuf>,
    /// Player records kept.
    pub players_kept: usize,
    /// Players whose state was written back on a join.
    pub players_restored: u64,
    /// Players with no identity under the `player_identity` rule (nothing kept for them).
    pub players_unidentified: u64,
    /// Recent warnings (newest last).
    pub warnings: Vec<PersistWarning>,
}

impl ServerPersistence {
    /// True when `[persistence] enabled` and the host started.
    pub fn active(&self) -> bool {
        self.active
    }

    /// The kept record of `identity`.
    pub fn player_record(&self, identity: &str) -> Option<&PlayerRecord> {
        self.store.get(identity)
    }

    /// Save counts by reason.
    pub fn counts(&self) -> SaveCounts {
        self.counts
    }

    /// The last save.
    pub fn last_save(&self) -> Option<&SaveReport> {
        self.last_save.as_ref()
    }

    /// The last restore.
    pub fn last_restore(&self) -> Option<&RestoreReport> {
        self.last_restore.as_ref()
    }

    /// Recent warnings (newest last).
    pub fn warnings(&self) -> &[PersistWarning] {
        &self.warnings
    }

    fn warn(&mut self, code: &'static str, message: String) {
        tracing::warn!(target: "regia_framework", code, "{message}");
        if self.warnings.len() >= MAX_WARNINGS {
            self.warnings.remove(0);
        }
        self.warnings.push(PersistWarning { code, message });
    }

    fn level_dir(&self, level: &str) -> PathBuf {
        self.root.join(level_folder(level))
    }
}

/// The folder name of a level: its path made file-name safe, plus a hash of the exact path so
/// `a/b` and `a_b` never share a folder.
pub fn level_folder(level: &str) -> String {
    let mut safe: String = level
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    if safe.is_empty() {
        safe.push('_');
    }
    // FNV-1a 32: a stable, dependency-free name hash (not a security boundary).
    let mut h: u32 = 0x811c_9dc5;
    for b in level.bytes() {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    format!("{safe}-{h:08x}")
}

/// Parses `world-<sequence>.save`.
fn parse_save_name(name: &str) -> Option<u64> {
    let digits = name.strip_prefix(SAVE_PREFIX)?.strip_suffix(SAVE_SUFFIX)?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

fn save_name(sequence: u64) -> String {
    format!("{SAVE_PREFIX}{sequence:012}{SAVE_SUFFIX}")
}

/// The saves of one level folder, newest first.
pub fn list_saves(dir: &Path) -> Vec<SaveEntry> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(sequence) = name.to_str().and_then(parse_save_name) else {
            continue;
        };
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        out.push(SaveEntry {
            sequence,
            path: entry.path(),
            bytes: meta.len(),
        });
    }
    out.sort_by_key(|a| std::cmp::Reverse(a.sequence));
    out
}

fn list_corrupt(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.to_str().is_some_and(|s| s.ends_with(CORRUPT_SUFFIX)))
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

/// The highest sequence in any level folder under `root`.
fn max_sequence(root: &Path) -> u64 {
    let mut max = 0;
    let Ok(levels) = std::fs::read_dir(root) else {
        return 0;
    };
    for level in levels.flatten() {
        if level.file_type().is_ok_and(|t| t.is_dir()) {
            if let Some(newest) = list_saves(&level.path()).first() {
                max = max.max(newest.sequence);
            }
            // A set-aside save still holds its number: never reuse it.
            for p in list_corrupt(&level.path()) {
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
                if let Some(seq) = name.strip_suffix(CORRUPT_SUFFIX).and_then(parse_save_name) {
                    max = max.max(seq);
                }
            }
        }
    }
    max
}

fn read_bytes(path: &Path) -> Result<Vec<u8>, String> {
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if meta.len() > MAX_SAVE_FILE_BYTES {
        return Err(format!(
            "{} bytes (the limit is {MAX_SAVE_FILE_BYTES})",
            meta.len()
        ));
    }
    std::fs::read(path).map_err(|e| e.to_string())
}

fn persistence(world: &mut World) -> Mut<'_, ServerPersistence> {
    if !world.contains_resource::<ServerPersistence>() {
        world.insert_resource(ServerPersistence::default());
    }
    world.resource_mut::<ServerPersistence>()
}

// ---------------------------------------------------------------------------
// Player records
// ---------------------------------------------------------------------------

/// The stable identity of `player` under `mode` (spec 77 § 4: the authenticator's platform id).
pub fn player_identity(
    world: &World,
    player: NetPlayerId,
    mode: PlayerIdentityMode,
) -> Option<(String, String)> {
    let server = world.get_resource::<NetSession>()?.server()?;
    let name = server.player_name(player).unwrap_or_default().to_owned();
    if let Some(pid) = server.peer_platform_id(player.peer) {
        // Two local players of one platform account share its id: the split-screen index tells
        // them apart (index 0 keeps the bare id).
        let id = if player.local == 0 {
            pid.to_owned()
        } else {
            format!("{pid}#{}", player.local)
        };
        return Some((id, name));
    }
    match mode {
        PlayerIdentityMode::PlatformOrName if !name.is_empty() => Some((format!("name:{name}"), name)),
        _ => None,
    }
}

fn registry_of(world: &World) -> AppTypeRegistry {
    world
        .get_resource::<AppTypeRegistry>()
        .cloned()
        .unwrap_or_default()
}

fn capture_components(
    world: &World,
    entity: Entity,
    types: &[(std::any::TypeId, &'static str)],
    registry: &TypeRegistry,
) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut codec = ReflectCodec::new(registry);
    for (type_id, path) in types {
        let Some(reg) = registry.get(*type_id) else {
            continue;
        };
        let Ok(value) = world.get_reflect(entity, *type_id) else {
            continue;
        };
        let short = reg.type_info().type_path_table().short_path().to_owned();
        match codec.to_postcard_typed(value.as_partial_reflect()) {
            Ok(bytes) => {
                out.insert(short, bytes);
            }
            Err(e) => tracing::warn!(target: "regia_framework", "player state {path}: cannot encode: {e}"),
        }
    }
    out
}

fn apply_components(
    world: &mut World,
    entity: Entity,
    saved: &BTreeMap<String, Vec<u8>>,
    registry: &TypeRegistry,
) -> usize {
    let mut codec = ReflectCodec::new(registry);
    let mut applied = 0;
    for (short, bytes) in saved {
        let Some(reg) = registry.get_with_short_type_path(short) else {
            tracing::warn!(target: "regia_framework", "player state {short}: type not registered; skipped");
            continue;
        };
        let Some(reflect_component) = reg.data::<ReflectComponent>() else {
            continue;
        };
        let value = match codec.from_postcard_typed(bytes, reg) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(target: "regia_framework", "player state {short}: cannot decode: {e}");
                continue;
            }
        };
        let Ok(mut e) = world.get_entity_mut(entity) else {
            return applied;
        };
        reflect_component.insert(&mut e, value.as_partial_reflect(), registry);
        applied += 1;
    }
    applied
}

fn pawn_of(world: &World, player_entity: Entity) -> Option<Entity> {
    world
        .get::<NetRulesJoined>(player_entity)
        .and_then(|j| j.pawn)
        .or_else(|| world.get::<PersistentPlayer>(player_entity).and_then(|p| p.pawn))
        .filter(|e| world.get_entity(*e).is_ok())
}

/// The record of the player on `player_entity` (`None` without a [`PersistentPlayer`] marker).
fn capture_player(world: &World, player_entity: Entity, stamp: u64) -> Option<PlayerRecord> {
    let marker = world.get::<PersistentPlayer>(player_entity)?;
    let types = world
        .get_resource::<PlayerPersistentComponents>()
        .map(|l| l.types.clone())
        .unwrap_or_default();
    let registry = registry_of(world);
    let registry = registry.read();
    let name = world
        .get::<regia_net::model::NetPlayer>(player_entity)
        .and_then(|p| {
            world
                .get_resource::<NetSession>()
                .and_then(|s| s.server())
                .and_then(|s| s.player_name(p.id))
                .map(str::to_owned)
        })
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| marker.name.clone());
    let pawn = pawn_of(world, player_entity)
        .map(|p| capture_components(world, p, &types, &registry))
        .unwrap_or_default();
    Some(PlayerRecord {
        identity: marker.identity.clone(),
        name,
        stamp,
        player: capture_components(world, player_entity, &types, &registry),
        pawn,
    })
}

/// Writes `record` back to the player entity and its pawn; returns the components applied.
fn apply_record(world: &mut World, player_entity: Entity, record: &PlayerRecord) -> usize {
    let registry = registry_of(world);
    let registry = registry.read();
    let mut applied = apply_components(world, player_entity, &record.player, &registry);
    if let Some(pawn) = pawn_of(world, player_entity) {
        applied += apply_components(world, pawn, &record.pawn, &registry);
    }
    applied
}

/// The join hook: looks the player up once and writes its kept state back.
pub fn persistence_player_joined(world: &mut World, player_entity: Entity, player: NetPlayerId) {
    let mode = {
        let Some(p) = world.get_resource::<ServerPersistence>() else {
            return;
        };
        if !p.active {
            return;
        }
        p.settings.player_identity
    };
    if world.get::<PersistentPlayer>(player_entity).is_some() {
        return;
    }
    let _span = tracing::debug_span!(target: "regia_framework", "server_persistence_join").entered();
    // The pawn comes from the rules' join hook: run it first when it is installed (it runs once
    // per player, so a later call in hook order is a no-op).
    let rules_installed = world.get_resource::<NetPlayerHooks>().is_some_and(|h| {
        h.joined()
            .iter()
            .any(|f| std::ptr::fn_addr_eq(*f, rules_player_joined as fn(&mut World, Entity, NetPlayerId)))
    });
    if rules_installed && world.get::<NetRulesJoined>(player_entity).is_none() {
        rules_player_joined(world, player_entity, player);
    }
    let Some((identity, name)) = player_identity(world, player, mode) else {
        persistence(world).players_unidentified += 1;
        tracing::info!(
            target: "regia_framework",
            peer = player.peer.0,
            local = player.local,
            "server persistence: player has no identity under the player_identity rule; nothing kept"
        );
        return;
    };
    let pawn = world.get::<NetRulesJoined>(player_entity).and_then(|j| j.pawn);
    if let Ok(mut e) = world.get_entity_mut(player_entity) {
        e.insert(PersistentPlayer {
            identity: identity.clone(),
            name,
            pawn,
        });
    }
    let record = world
        .get_resource::<ServerPersistence>()
        .and_then(|p| p.store.get(&identity).cloned());
    if let Some(record) = record {
        let applied = apply_record(world, player_entity, &record);
        persistence(world).players_restored += 1;
        tracing::info!(target: "regia_framework", %identity, applied, "server persistence: player state restored");
    }
}

/// The leave hook: keeps the leaving player's state and asks for a save at the end of the tick.
pub fn persistence_player_left(world: &mut World, player_entity: Entity, _player: NetPlayerId) {
    let stamp = {
        let Some(p) = world.get_resource::<ServerPersistence>() else {
            return;
        };
        if !p.active {
            return;
        }
        p.next_sequence
    };
    let Some(record) = capture_player(world, player_entity, stamp) else {
        return;
    };
    let mut p = persistence(world);
    tracing::info!(target: "regia_framework", identity = %record.identity, "server persistence: player state kept");
    p.store.insert(record.identity.clone(), record);
    p.pending_player_save = true;
}

// ---------------------------------------------------------------------------
// The save section
// ---------------------------------------------------------------------------

fn write_player_section(world: &mut World) -> Result<Option<Vec<u8>>, SaveError> {
    let Some((sequence, reason)) = world.get_resource::<ServerPersistence>().and_then(|p| p.writing) else {
        // Not a server persistence save (a game's own save): no section.
        return Ok(None);
    };
    // Players still in the game: their current state goes into this save.
    let live: Vec<Entity> = world
        .query_filtered::<Entity, With<PersistentPlayer>>()
        .iter(world)
        .collect();
    let records: Vec<PlayerRecord> = live
        .into_iter()
        .filter_map(|e| capture_player(world, e, sequence))
        .collect();
    let mut p = persistence(world);
    for r in records {
        p.store.insert(r.identity.clone(), r);
    }
    let section = PlayerSection {
        format_version: PLAYER_SECTION_FORMAT_VERSION,
        level: p.level.clone(),
        tick: p.tick,
        sequence,
        reason,
        players: p.store.values().cloned().collect(),
    };
    section.to_bytes().map(Some)
}

fn read_player_section(world: &mut World, bytes: Option<&[u8]>) -> Result<(), SaveError> {
    let Some(p) = world.get_resource::<ServerPersistence>() else {
        return Ok(());
    };
    if !p.active {
        return Ok(());
    }
    let section = match bytes.map(PlayerSection::from_bytes).transpose() {
        Ok(s) => s,
        Err(e) => {
            persistence(world).warn("persistence_player_section_unreadable", e.to_string());
            return Err(e);
        }
    };
    let mut p = persistence(world);
    merge_section(&mut p, section.as_ref());
    p.read_section = section;
    Ok(())
}

fn merge_section(p: &mut ServerPersistence, section: Option<&PlayerSection>) {
    if p.merge == MergeMode::Replace {
        p.store.clear();
    }
    let Some(section) = section else { return };
    for r in &section.players {
        let newer = p.store.get(&r.identity).is_none_or(|old| r.stamp > old.stamp);
        if newer {
            p.store.insert(r.identity.clone(), r.clone());
        }
    }
}

// ---------------------------------------------------------------------------
// Save / restore
// ---------------------------------------------------------------------------

/// Saves the world now into the current level's folder (atomic write, then rotation).
pub fn save_now(world: &mut World, reason: SaveReason) -> Result<SaveReport, PersistError> {
    let _span = tracing::info_span!(target: "regia_framework", "server_persistence_save").entered();
    let (dir, sequence, level, tick, keep) = {
        let Some(p) = world.get_resource::<ServerPersistence>() else {
            return Err(PersistError::new(
                "persistence_disabled",
                "the server persistence plugin is not installed",
            ));
        };
        if !p.active {
            return Err(PersistError::new(
                "persistence_disabled",
                "[persistence] enabled is false (or the host has not started)",
            ));
        }
        (
            p.level_dir(&p.level),
            p.next_sequence,
            p.level.clone(),
            p.tick,
            p.settings.keep,
        )
    };
    persistence(world).writing = Some((sequence, reason));
    let encoded = SaveGame::encode_counted(world, &SaveSpec::default());
    let players = persistence(world).store.len();
    persistence(world).writing = None;
    let (bytes, counted) = match encoded {
        Ok(v) => v,
        Err(e) => {
            let mut p = persistence(world);
            p.counts.failed += 1;
            let err = PersistError::new("persistence_encode_failed", e.to_string());
            p.warn("persistence_write_failed", err.message.clone());
            return Err(err);
        }
    };
    let path = dir.join(save_name(sequence));
    if let Err(e) = write_atomic(&path, &bytes) {
        let mut p = persistence(world);
        p.counts.failed += 1;
        let message = e.to_string();
        p.warn("persistence_write_failed", message.clone());
        return Err(PersistError::new("persistence_write_failed", message));
    }
    // Rotation: the oldest saves beyond `keep` in this folder go (set-aside corrupt files stay).
    let mut removed = 0;
    let saves = list_saves(&dir);
    let mut rotation_errors = Vec::new();
    for old in saves.iter().skip(keep.max(1) as usize) {
        match std::fs::remove_file(&old.path) {
            Ok(()) => removed += 1,
            Err(e) => rotation_errors.push(format!("{}: {e}", old.path.display())),
        }
    }
    let report = SaveReport {
        reason,
        sequence,
        level,
        tick,
        path,
        bytes: bytes.len(),
        entities: counted.entity_count,
        players,
        removed,
    };
    let mut p = persistence(world);
    for e in rotation_errors {
        p.warn("persistence_rotation_failed", e);
    }
    p.next_sequence = sequence + 1;
    p.counts.bump(reason);
    // Any save holds the records of players who left: nothing is pending after it.
    p.pending_player_save = false;
    tracing::info!(
        target: "regia_framework",
        reason = reason.as_str(),
        sequence,
        tick,
        bytes = report.bytes,
        entities = report.entities,
        players,
        removed,
        "server persistence: saved"
    );
    p.last_save = Some(report.clone());
    Ok(report)
}

/// Applies one save file (`merge` for the player records). The world is untouched when it fails.
fn apply_file(
    world: &mut World,
    path: &Path,
    merge: MergeMode,
) -> Result<(usize, Option<PlayerSection>), String> {
    let bytes = read_bytes(path)?;
    {
        let mut p = persistence(world);
        p.merge = merge;
        p.read_section = None;
    }
    let result = SaveGame::decode(world, &bytes, &SaveSpec::default());
    let mut p = persistence(world);
    p.merge = MergeMode::KeepNewer;
    let section = p.read_section.take();
    match result {
        Ok(r) => Ok((r.entity_count, section)),
        Err(e) => Err(e.to_string()),
    }
}

/// Restores the newest good save of `level` (falling back past bad ones). `refuse_if_none_good`:
/// when the folder has saves and none decodes, fail (start) instead of setting them aside and
/// carrying on fresh (a level change).
fn restore_level(
    world: &mut World,
    level: &str,
    refuse_if_none_good: bool,
) -> Result<RestoreReport, PersistError> {
    let _span = tracing::info_span!(target: "regia_framework", "server_persistence_restore").entered();
    let dir = persistence(world).level_dir(level);
    let saves = list_saves(&dir);
    let mut failed: Vec<(PathBuf, String)> = Vec::new();
    for entry in &saves {
        match apply_file(world, &entry.path, MergeMode::KeepNewer) {
            Ok((entities, section)) => {
                // Set the bad newer saves aside so the rotation never removes the good one first.
                let mut fell_back = Vec::new();
                let mut warnings = Vec::new();
                for (path, why) in &failed {
                    let aside = corrupt_name(path);
                    match std::fs::rename(path, &aside) {
                        Ok(()) => {
                            warnings.push(format!(
                                "{}: {why}; set aside as {}, restored the older save {}",
                                path.display(),
                                aside.display(),
                                entry.path.display()
                            ));
                            fell_back.push(aside);
                        }
                        Err(e) => {
                            warnings.push(format!("{}: {why}; cannot set it aside: {e}", path.display()));
                            fell_back.push(path.clone());
                        }
                    }
                }
                let mut p = persistence(world);
                for w in warnings {
                    p.warn("persistence_save_corrupt", w);
                }
                let report = RestoreReport {
                    level: level.to_owned(),
                    sequence: Some(entry.sequence),
                    path: Some(entry.path.clone()),
                    saved_tick: section.as_ref().map(|s| s.tick),
                    entities,
                    fell_back,
                    players: p.store.len(),
                };
                tracing::info!(
                    target: "regia_framework",
                    level,
                    sequence = entry.sequence,
                    entities,
                    "server persistence: restored"
                );
                p.last_restore = Some(report.clone());
                return Ok(report);
            }
            Err(why) => failed.push((entry.path.clone(), why)),
        }
    }
    if !failed.is_empty() {
        let list = failed
            .iter()
            .map(|(p, why)| format!("{}: {why}", p.display()))
            .collect::<Vec<_>>()
            .join("; ");
        if refuse_if_none_good {
            let err = PersistError::new(
                "persistence_no_good_save",
                format!(
                    "no save of level `{level}` in {} decodes ({list}); move the folder away to start fresh",
                    dir.display()
                ),
            );
            persistence(world).warn("persistence_no_good_save", err.message.clone());
            return Err(err);
        }
        let mut fell_back = Vec::new();
        for (path, _) in &failed {
            let aside = corrupt_name(path);
            if std::fs::rename(path, &aside).is_ok() {
                fell_back.push(aside);
            }
        }
        persistence(world).warn(
            "persistence_no_good_save",
            format!("no save of level `{level}` decodes ({list}); set aside, the level starts fresh"),
        );
        let mut p = persistence(world);
        let report = RestoreReport {
            level: level.to_owned(),
            sequence: None,
            path: None,
            saved_tick: None,
            entities: 0,
            fell_back,
            players: p.store.len(),
        };
        p.last_restore = Some(report.clone());
        return Ok(report);
    }
    let mut p = persistence(world);
    let report = RestoreReport {
        level: level.to_owned(),
        sequence: None,
        path: None,
        saved_tick: None,
        entities: 0,
        fell_back: Vec::new(),
        players: p.store.len(),
    };
    p.last_restore = Some(report.clone());
    Ok(report)
}

fn corrupt_name(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_default();
    name.push(CORRUPT_SUFFIX);
    path.with_file_name(name)
}

/// Merges the player records of every other level's newest good save (newest record wins).
fn merge_other_levels(world: &mut World, level: &str) {
    let root = persistence(world).root.clone();
    let own = level_folder(level);
    let Ok(levels) = std::fs::read_dir(&root) else {
        return;
    };
    let mut dirs: Vec<PathBuf> = levels
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()) && e.file_name().to_str() != Some(own.as_str()))
        .map(|e| e.path())
        .collect();
    dirs.sort();
    for dir in dirs {
        for entry in list_saves(&dir) {
            let slot = SaveSlot {
                name: save_name(entry.sequence),
                path: entry.path.clone(),
                header: None,
            };
            let raw = match SaveGame::load_raw(&slot, world) {
                Ok(raw) => raw,
                Err(_) => continue,
            };
            let Some(bytes) = raw.resources.iter().find(|r| r.type_path == PLAYER_SECTION_KEY) else {
                break;
            };
            match PlayerSection::from_bytes(&bytes.data) {
                Ok(section) => {
                    let mut p = persistence(world);
                    p.merge = MergeMode::KeepNewer;
                    merge_section(&mut p, Some(&section));
                }
                Err(e) => persistence(world).warn(
                    "persistence_player_section_unreadable",
                    format!("{}: {e}", entry.path.display()),
                ),
            }
            break;
        }
    }
}

/// Restores slot `sequence` of the current level (an operator rollback): the world and every
/// player record come from that save; players in the game get their saved state written back.
/// The world is untouched when the save does not decode.
pub fn restore_slot(world: &mut World, sequence: u64) -> Result<RestoreReport, PersistError> {
    let _span = tracing::info_span!(target: "regia_framework", "server_persistence_restore_slot").entered();
    let (dir, level) = {
        let Some(p) = world.get_resource::<ServerPersistence>() else {
            return Err(PersistError::new(
                "persistence_disabled",
                "the server persistence plugin is not installed",
            ));
        };
        if !p.active {
            return Err(PersistError::new(
                "persistence_disabled",
                "[persistence] enabled is false",
            ));
        }
        (p.level_dir(&p.level), p.level.clone())
    };
    let Some(entry) = list_saves(&dir).into_iter().find(|e| e.sequence == sequence) else {
        return Err(PersistError::new(
            "persistence_slot_unknown",
            format!("no save {sequence} of level `{level}` in {}", dir.display()),
        ));
    };
    let (entities, section) = apply_file(world, &entry.path, MergeMode::Replace).map_err(|why| {
        PersistError::new(
            "persistence_save_corrupt",
            format!("{}: {why}", entry.path.display()),
        )
    })?;
    // Players in the game take the restored state too.
    let live: Vec<(Entity, String)> = world
        .query::<(Entity, &PersistentPlayer)>()
        .iter(world)
        .map(|(e, p)| (e, p.identity.clone()))
        .collect();
    for (entity, identity) in live {
        let record = persistence(world).store.get(&identity).cloned();
        if let Some(record) = record {
            apply_record(world, entity, &record);
        }
    }
    let mut p = persistence(world);
    let report = RestoreReport {
        level,
        sequence: Some(sequence),
        path: Some(entry.path),
        saved_tick: section.map(|s| s.tick),
        entities,
        fell_back: Vec::new(),
        players: p.store.len(),
    };
    p.last_restore = Some(report.clone());
    Ok(report)
}

/// The status (`server.persistence_status`); `None` without the plugin.
pub fn status(world: &World) -> Option<PersistenceStatus> {
    let p = world.get_resource::<ServerPersistence>()?;
    let dir = p.level_dir(&p.level);
    let every = u64::from(p.settings.autosave_ticks);
    Some(PersistenceStatus {
        enabled: p.active,
        settings: p.settings.clone(),
        root: p.root.clone(),
        level: p.level.clone(),
        tick: p.tick,
        next_autosave_tick: if p.active && every > 0 {
            Some((p.tick / every + 1) * every)
        } else {
            None
        },
        next_sequence: p.next_sequence,
        counts: p.counts,
        last_save: p.last_save.clone(),
        last_restore: p.last_restore.clone(),
        saves: if p.active { list_saves(&dir) } else { Vec::new() },
        corrupt: if p.active { list_corrupt(&dir) } else { Vec::new() },
        players_kept: p.store.len(),
        players_restored: p.players_restored,
        players_unidentified: p.players_unidentified,
        warnings: p.warnings.clone(),
    })
}

// ---------------------------------------------------------------------------
// Host events and console
// ---------------------------------------------------------------------------

/// Starts persistence from the host's settings and restores the level's newest good save.
pub fn start(
    world: &mut World,
    settings: &PersistenceSettings,
    base_dir: &Path,
    level: &str,
) -> Result<Option<RestoreReport>, PersistError> {
    {
        let mut p = persistence(world);
        p.settings = settings.clone();
        p.active = settings.enabled;
        p.root = base_dir.join(&settings.dir);
        p.level = level.to_owned();
        p.tick = 0;
    }
    if !settings.enabled {
        return Ok(None);
    }
    let root = persistence(world).root.clone();
    if let Err(e) = std::fs::create_dir_all(&root) {
        return Err(PersistError::new(
            "persistence_write_failed",
            format!("cannot create {}: {e}", root.display()),
        ));
    }
    persistence(world).next_sequence = max_sequence(&root) + 1;
    let report = restore_level(world, level, true)?;
    merge_other_levels(world, level);
    let players = persistence(world).store.len();
    let mut report = report;
    report.players = players;
    if let Some(r) = persistence(world).last_restore.as_mut() {
        r.players = players;
    }
    Ok(Some(report))
}

fn host_event_hook(world: &mut World, event: &HostEvent<'_>) -> Result<(), String> {
    match *event {
        HostEvent::Started {
            level,
            base_dir,
            settings,
        } => start(world, &settings.persistence, base_dir, level)
            .map(|_| ())
            .map_err(|e| e.to_string()),
        HostEvent::Ticked { tick } => {
            let (active, every, pending) = {
                let mut p = persistence(world);
                p.tick = tick;
                (
                    p.active,
                    u64::from(p.settings.autosave_ticks),
                    p.pending_player_save,
                )
            };
            if !active {
                return Ok(());
            }
            let reason = if every > 0 && tick.is_multiple_of(every) {
                Some(SaveReason::Autosave)
            } else if pending {
                Some(SaveReason::PlayerLeft)
            } else {
                None
            };
            match reason {
                Some(r) => save_now(world, r).map(|_| ()).map_err(|e| e.to_string()),
                None => Ok(()),
            }
        }
        HostEvent::LevelLeaving { to, .. } => {
            let (active, on_change) = {
                let p = persistence(world);
                (p.active, p.settings.save_on_level_change)
            };
            if !active {
                return Ok(());
            }
            let result = if on_change {
                save_now(world, SaveReason::LevelChange).map(|_| ())
            } else {
                Ok(())
            };
            persistence(world).level = to.to_owned();
            result.map_err(|e| e.to_string())
        }
        HostEvent::LevelLoaded { level } => {
            if !persistence(world).active {
                return Ok(());
            }
            persistence(world).level = level.to_owned();
            restore_level(world, level, false)
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
        HostEvent::Stopping { tick } => {
            let (active, on_stop) = {
                let mut p = persistence(world);
                p.tick = tick;
                (p.active, p.settings.save_on_shutdown)
            };
            if active && on_stop {
                save_now(world, SaveReason::Shutdown)
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            } else {
                Ok(())
            }
        }
        _ => Ok(()),
    }
}

fn console_save(world: &mut World, _rest: &str) -> Vec<String> {
    match save_now(world, SaveReason::Manual) {
        Ok(r) => vec![format!(
            "saved {} ({} bytes, {} entities, {} players, tick {})",
            r.path.display(),
            r.bytes,
            r.entities,
            r.players,
            r.tick
        )],
        Err(e) => vec![format!("error: {e}")],
    }
}

fn console_status(world: &mut World, _rest: &str) -> Vec<String> {
    let Some(s) = status(world) else {
        return vec!["error: persistence_disabled: the plugin is not installed".into()];
    };
    let mut out = vec![format!(
        "enabled={} level={} tick={} next_autosave={} next_slot={} saves: autosave={} shutdown={} level_change={} player_left={} manual={} failed={} players_kept={}",
        s.enabled,
        s.level,
        s.tick,
        s.next_autosave_tick
            .map_or_else(|| "off".to_owned(), |t| t.to_string()),
        s.next_sequence,
        s.counts.autosave,
        s.counts.shutdown,
        s.counts.level_change,
        s.counts.player_left,
        s.counts.manual,
        s.counts.failed,
        s.players_kept
    )];
    for save in &s.saves {
        out.push(format!(
            "slot {} {} bytes {}",
            save.sequence,
            save.bytes,
            save.path.display()
        ));
    }
    for w in &s.warnings {
        out.push(format!("warning {}: {}", w.code, w.message));
    }
    out
}

fn console_restore(world: &mut World, rest: &str) -> Vec<String> {
    let mut parts = rest.split_whitespace();
    let Some(sequence) = parts.next().and_then(|s| s.parse::<u64>().ok()) else {
        return vec!["error: usage: restore <slot> confirm (slots: `persistence`)".into()];
    };
    if parts.next() != Some("confirm") {
        return vec![format!(
            "error: confirmation_required: restore replaces the running world; type `restore {sequence} confirm`"
        )];
    }
    match restore_slot(world, sequence) {
        Ok(r) => vec![format!(
            "restored slot {sequence} ({} entities, {} player records)",
            r.entities, r.players
        )],
        Err(e) => vec![format!("error: {e}")],
    }
}

/// Keeps a dedicated server's world across restarts (see the module docs). Add it to the server
/// app next to `NetSessionPlugin` and the save framework; it acts only when the host's
/// `server.toml` has `[persistence] enabled = true`.
#[derive(Debug, Default, Clone, Copy)]
pub struct ServerPersistencePlugin;

impl Plugin for ServerPersistencePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ServerPersistence>()
            .init_resource::<PlayerPersistentComponents>()
            .init_resource::<SaveSections>();
        let world = app.world_mut();
        world.resource_mut::<SaveSections>().register(
            PLAYER_SECTION_KEY,
            write_player_section,
            read_player_section,
        );
        let mut hooks = world.get_resource_or_insert_with(HostGameHooks::default);
        hooks.add_event_hook(host_event_hook);
        hooks.add_command(
            "save",
            "save                            save the world now (persistence)",
            console_save,
        );
        hooks.add_command(
            "persistence",
            "persistence                     saves, counts, slots and warnings",
            console_status,
        );
        hooks.add_command(
            "restore",
            "restore <slot> confirm          roll the world back to a save slot",
            console_restore,
        );
        let mut players = world.get_resource_or_insert_with(NetPlayerHooks::default);
        players.add_joined(persistence_player_joined);
        players.add_left(persistence_player_left);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_names_round_trip_and_folders_are_distinct() {
        assert_eq!(parse_save_name(&save_name(42)), Some(42));
        assert_eq!(parse_save_name("world-.save"), None);
        assert_eq!(parse_save_name("world-12x.save"), None);
        assert_eq!(parse_save_name("world-000000000001.save.corrupt"), None);
        assert_ne!(level_folder("a/b"), level_folder("a_b"));
        assert_eq!(
            level_folder("levels/x.level.ron"),
            level_folder("levels/x.level.ron")
        );
        assert!(level_folder("../../etc").chars().all(|c| c != '/'));
    }

    #[test]
    fn the_section_round_trips_and_refuses_newer_versions_and_duplicates() {
        let record = PlayerRecord {
            identity: "steam:1".into(),
            name: "Ann".into(),
            stamp: 3,
            player: BTreeMap::from([("Inventory".to_owned(), vec![1, 2, 3])]),
            pawn: BTreeMap::new(),
        };
        let section = PlayerSection {
            format_version: PLAYER_SECTION_FORMAT_VERSION,
            level: "l".into(),
            tick: 60,
            sequence: 3,
            reason: SaveReason::Autosave,
            players: vec![record.clone()],
        };
        let bytes = section.to_bytes().expect("encodes");
        assert_eq!(PlayerSection::from_bytes(&bytes).expect("decodes"), section);
        let newer = PlayerSection {
            format_version: 2,
            ..section.clone()
        };
        assert!(PlayerSection::from_bytes(&newer.to_bytes().expect("encodes")).is_err());
        let twice = PlayerSection {
            players: vec![record.clone(), record],
            ..section
        };
        assert!(PlayerSection::from_bytes(&twice.to_bytes().expect("encodes")).is_err());
        assert!(PlayerSection::from_bytes(&bytes[..bytes.len() / 2]).is_err());
    }

    #[test]
    fn a_merge_keeps_the_newer_record_unless_replacing() {
        let rec = |stamp: u64, v: u8| PlayerRecord {
            identity: "steam:1".into(),
            name: String::new(),
            stamp,
            player: BTreeMap::from([("X".to_owned(), vec![v])]),
            pawn: BTreeMap::new(),
        };
        let section = |r: PlayerRecord| PlayerSection {
            format_version: 1,
            level: String::new(),
            tick: 0,
            sequence: r.stamp,
            reason: SaveReason::Manual,
            players: vec![r],
        };
        let mut p = ServerPersistence::default();
        merge_section(&mut p, Some(&section(rec(5, 5))));
        merge_section(&mut p, Some(&section(rec(3, 3))));
        assert_eq!(p.store["steam:1"].stamp, 5, "an older record does not win");
        p.merge = MergeMode::Replace;
        merge_section(&mut p, Some(&section(rec(3, 3))));
        assert_eq!(p.store["steam:1"].stamp, 3, "a restore replaces");
        merge_section(&mut p, None);
        assert!(p.store.is_empty(), "a save without the section holds no players");
    }
}
