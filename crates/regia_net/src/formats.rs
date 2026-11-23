// SPDX-License-Identifier: MPL-2.0
//! Schema fingerprints of the versioned files `regia_net` reads, for `cargo xtask check-formats`
//! (`docs/specs/12-assets.md` § 4a, ADR-2316): `config/net.ron`, `.netprofile.ron`, `server.toml`,
//! `bans.ron`, `net-manifest.ron`, `server.key`, `*.siminput.ron`, the engine's lobby keys, the
//! version probe's datagram layout and the lockstep desync bundle `*.desync.ron` (ADR-8455).
//!
//! Each fingerprint lists the serde field names of the format's root struct and of every struct it
//! reaches, plus the variant names and payload shapes of every enum it reaches, so a field rename
//! changes the hash and the gate refuses it without a version bump. `server.toml` is read key by key
//! (no serde struct): its fingerprint is [`SERVER_TOML_KEYS`], which a test here pins to the parser.
//! The lobby keys are a key/value layout on the lobby platform: their fingerprint is
//! [`crate::lobby::keys::ALL`] plus the state and server spellings.
//!
//! Configuration migrations: `config/net.ron` v1 → v2 and `server.toml` v1/v2 → v3
//! (ADR-4854). Other formats here remain at v1.

use regia_core::schema::{join_field_lists, serde_field_names, serde_variant_names, serde_variant_shapes};
use serde::Deserialize;

/// The field names and payload shapes of the serde enum `T`, as one fingerprint list.
fn enum_list<T: Deserialize<'static>>() -> Vec<String> {
    let mut list = serde_variant_names::<T>();
    list.extend(serde_variant_shapes::<T>());
    list
}

/// `config/net.ron` (`NetSettings`, `docs/formats/net-ron.md`): the root and its eight sections.
#[must_use]
pub fn net_settings_schema_fingerprint() -> String {
    use crate::settings::{
        InterpolationSettings, LagCompSettings, MessageSettings, NetSettings, PredictionSettings, RateLimit,
        ReplicationSettings, StateChannel,
    };
    join_field_lists(&[
        serde_field_names::<NetSettings>(),
        serde_field_names::<MessageSettings>(),
        serde_field_names::<RateLimit>(),
        serde_field_names::<ReplicationSettings>(),
        enum_list::<StateChannel>(),
        enum_list::<crate::entropy::EntropyMode>(),
        serde_field_names::<InterpolationSettings>(),
        serde_field_names::<crate::session::SessionSettings>(),
        serde_field_names::<crate::cc::RateSettings>(),
        enum_list::<crate::crypto::CryptoMode>(),
        serde_field_names::<PredictionSettings>(),
        serde_field_names::<LagCompSettings>(),
        serde_field_names::<crate::lobby::LobbySettings>(),
        enum_list::<crate::lobby::LobbyVisibility>(),
        enum_list::<crate::lobby::query::Distance>(),
    ])
}

/// `config/net.ron` v1 → v2 (ADR-4854), v2 → v3 (ADR-7950) and v3 → v4 (ADR-10200, the
/// `session.rate_control.first_flight` key): [`crate::settings::migrate_net_settings`].
#[must_use]
pub fn net_settings_migration_sources() -> Vec<u32> {
    vec![1, 2, 3]
}

/// `.netprofile.ron` (`NetProfile`, `docs/formats/netprofile-ron.md`). Net profiles are ECS assets
/// (feature `ecs`, ADR-8032).
#[cfg(feature = "ecs")]
#[must_use]
pub fn net_profile_schema_fingerprint() -> String {
    use crate::profile::{InterpolationMode, NetProfile, RelevanceShape};
    join_field_lists(&[
        serde_field_names::<NetProfile>(),
        enum_list::<RelevanceShape>(),
        enum_list::<InterpolationMode>(),
    ])
}

/// `.netprofile.ron` v1 → v2 (the optional `rate_tiers`, ADR-8031, and `extrapolation_gate_m`, off
/// when absent, ADR-8522; one unreleased v2, ADR-8572): a version-number step.
#[must_use]
pub fn net_profile_migration_sources() -> Vec<u32> {
    vec![1]
}

/// Every key `server.toml` v1 reads, `section.key` (and the top-level `format_version`), in the
/// order of `docs/formats/server-toml.md`. The parser reads the file key by key rather than through a
/// serde struct, so this list is the format's shape; a test pins it to the parser both ways (every
/// listed key is read without a warning, and the parser's settings serialize to exactly these keys).
pub const SERVER_TOML_KEYS: &[&str] = &[
    "format_version",
    "server.name",
    "server.bind",
    "server.port",
    "server.max_players",
    "server.password",
    "server.tick_hz",
    "server.motd",
    "server.idle_tick_hz",
    "levels.rotation",
    "levels.playlist",
    "levels.start_index",
    "levels.match_end_delay_s",
    "net.client_rate_bytes",
    "net.server_rate_bytes",
    "net.send_every_ticks",
    "net.encryption",
    "net.identity_file",
    "admin.token_file",
    "status.enabled",
    "status.bind",
    "status.port",
    "logs.dir",
    "logs.keep_files",
    // v2 (ADR-3354).
    "persistence.enabled",
    "persistence.dir",
    "persistence.autosave_ticks",
    "persistence.keep",
    "persistence.save_on_shutdown",
    "persistence.save_on_level_change",
    "persistence.player_identity",
    "bots.min_players",
    "bots.max_bots",
    "bots.join_delay_ticks",
    "bots.leave_delay_ticks",
    "bots.seed",
    "bots.reaction_ticks",
    "bots.aim_error_radians",
    "bots.perception_radius",
    "net.entropy",
    "net.interrupt_ms",
    "net.timeout_ms",
    "net.start_grace_ms",
    "net.mtu_probing",
    // `[lockstep]`, folded into the unreleased v4 (ADR-8558 under ADR-7724).
    "lockstep.enabled",
    "lockstep.max_seats",
    "lockstep.input_delay_ticks",
    "lockstep.observer",
    "lockstep.checkpoint_ticks",
    "lockstep.observer_max_lag_ticks",
    "lockstep.observer_max_backlog_ticks",
];

/// `server.toml` (`docs/formats/server-toml.md`): [`SERVER_TOML_KEYS`], the `net.encryption`
/// spellings and the `persistence.player_identity` spellings. The dedicated-server host is ECS
/// (feature `ecs`, ADR-8032).
#[cfg(feature = "ecs")]
#[must_use]
pub fn server_toml_schema_fingerprint() -> String {
    use crate::server_host::config::PlayerIdentityMode;
    join_field_lists(&[
        SERVER_TOML_KEYS.iter().map(|k| (*k).to_owned()).collect(),
        enum_list::<crate::crypto::CryptoMode>(),
        enum_list::<crate::entropy::EntropyMode>(),
        [
            crate::entropy::EntropyMode::Off,
            crate::entropy::EntropyMode::Range,
        ]
        .iter()
        .map(|mode| mode.as_str().to_owned())
        .collect(),
        [PlayerIdentityMode::Platform, PlayerIdentityMode::PlatformOrName]
            .iter()
            .map(|m| m.as_str().to_owned())
            .collect(),
    ])
}

/// `server.toml` v1/v2 → v3 (ADR-4854) and v3 → v4 (`levels.playlist`, ADR-8345):
/// `server_host::config::migrate_server_toml`.
#[must_use]
pub fn server_toml_migration_sources() -> Vec<u32> {
    vec![1, 2, 3]
}

/// `.playlist.ron` version this build writes (v2, ADR-10064; v1 was ADR-8345). v2 has v1's shape;
/// its reader also refuses an unknown field inside `order` (`shuffle(seed: 1, typo: 2)`), and the
/// schema fingerprint now covers the `order` variants and their payloads. v1 still loads: the step
/// only rewrites the version number.
pub const PLAYLIST_RON_FORMAT_VERSION: u32 = 2;

/// One `.playlist.ron` file: ordered map and mode rows (`docs/formats/playlist-ron.md`).
///
/// Unknown fields are refused, in the file and in each row (ADR-8553).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlaylistFile {
    /// `format_version`.
    pub format_version: u32,
    /// Start over at the first row after the last.
    #[serde(default)]
    pub looping: bool,
    /// How the rows are ordered (ADR-8897): `in_order` (the default), `shuffle(seed: N)` or
    /// `weighted(seed: N)`. Not written at its default.
    #[serde(
        default,
        skip_serializing_if = "regia_rules::playlist::PlaylistOrder::is_in_order",
        deserialize_with = "strict_order"
    )]
    pub order: regia_rules::playlist::PlaylistOrder,
    /// The rotation avoids the last `n` rows played while another row fits (ADR-8897). `0` (the
    /// default) is no guard. Not written at its default.
    #[serde(default, skip_serializing_if = "is_zero_u8")]
    pub no_repeat_last: u8,
    /// The rows, in list order.
    pub entries: Vec<regia_rules::playlist::PlaylistEntry>,
}

fn is_zero_u8(value: &u8) -> bool {
    *value == 0
}

/// The `.playlist.ron` reader of [`regia_rules::playlist::PlaylistOrder`]: the same wire names, plus
/// `deny_unknown_fields` (ADR-10064). The shared type stays lenient for session snapshots and the
/// agent protocol; a playlist file refuses a typo inside `order` the way it refuses one anywhere
/// else. The conversions destructure exhaustively, so a variant or field added to the shared type
/// stops this compiling until the mirror has it too.
#[derive(Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum PlaylistOrderFile {
    InOrder,
    Shuffle { seed: u64 },
    Weighted { seed: u64 },
}

impl From<PlaylistOrderFile> for regia_rules::playlist::PlaylistOrder {
    fn from(file: PlaylistOrderFile) -> Self {
        match file {
            PlaylistOrderFile::InOrder => Self::InOrder,
            PlaylistOrderFile::Shuffle { seed } => Self::Shuffle { seed },
            PlaylistOrderFile::Weighted { seed } => Self::Weighted { seed },
        }
    }
}

impl From<regia_rules::playlist::PlaylistOrder> for PlaylistOrderFile {
    fn from(order: regia_rules::playlist::PlaylistOrder) -> Self {
        use regia_rules::playlist::PlaylistOrder;
        match order {
            PlaylistOrder::InOrder => Self::InOrder,
            PlaylistOrder::Shuffle { seed } => Self::Shuffle { seed },
            PlaylistOrder::Weighted { seed } => Self::Weighted { seed },
        }
    }
}

fn strict_order<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<regia_rules::playlist::PlaylistOrder, D::Error> {
    PlaylistOrderFile::deserialize(d).map(Into::into)
}

/// The `.playlist.ron` versions [`PlaylistFile::from_ron`] reads: v1 (same shape, ADR-8345) and the
/// current v2.
fn playlist_version_readable(found: u32) -> bool {
    found == 1 || found == PLAYLIST_RON_FORMAT_VERSION
}

impl PlaylistFile {
    /// Parses RON. A v1 file is read as v2 (the shape did not change; the value's `format_version`
    /// is the current one, so writing it back writes v2). Refuses a `format_version` this build does
    /// not read (read before the full parse, so a newer file whose body changed still names its
    /// version), an unknown field (also inside `order`, ADR-10064), an
    /// empty row list, a row with an empty `map_id`, a row weight above
    /// `regia_rules::playlist::MAX_ROW_WEIGHT` and a row whose `min_players` is above its
    /// `max_players` (ADR-8897), each bound read as the rotation reads it: the field, else the
    /// option of that name.
    pub fn from_ron(text: &str) -> Result<Self, String> {
        // A peek that fails (no top-level map, no version) falls through to the full parse, whose
        // error is the precise one.
        if let Ok(found) = regia_core::version::peek_ron_version(text)
            && !playlist_version_readable(found)
        {
            return Err(no_playlist_migrator(found));
        }
        // Row gates may be written bare (`min_players: 4`) or as `Some(4)` (ADR-8897), as in
        // `.rules.ron`.
        let mut file: Self = ron::Options::default()
            .with_default_extension(ron::extensions::Extensions::IMPLICIT_SOME)
            .from_str(text)
            .map_err(|e| e.to_string())?;
        if !playlist_version_readable(file.format_version) {
            return Err(no_playlist_migrator(file.format_version));
        }
        // v1 -> v2 changes no field: the step is the number (ADR-10064).
        file.format_version = PLAYLIST_RON_FORMAT_VERSION;
        if file.entries.is_empty() {
            return Err("playlist.ron: at least one row".into());
        }
        if let Some(row) = file.entries.iter().position(|e| e.map_id.trim().is_empty()) {
            return Err(format!("playlist.ron: row {row} has an empty map_id"));
        }
        // ADR-8897: a weight above the cap and an empty player gate are mistakes, not silent rules.
        let max_weight = regia_rules::playlist::MAX_ROW_WEIGHT;
        if let Some(row) = file.entries.iter().position(|e| e.weight > max_weight) {
            return Err(format!("playlist.ron: row {row} has a weight above {max_weight}"));
        }
        // The gate the rotation runs reads each bound from the field, else the `min_players` /
        // `max_players` option, so an inverted pair is refused whichever way it was written.
        if let Some(row) = file.entries.iter().position(
            |e| matches!((e.min_players_bound(), e.max_players_bound()), (Some(min), Some(max)) if min > max),
        ) {
            return Err(format!(
                "playlist.ron: row {row} has min_players above max_players"
            ));
        }
        Ok(file)
    }

    /// Writes pretty RON.
    pub fn to_ron(&self) -> Result<String, String> {
        ron::ser::to_string_pretty(self, regia_core::ron_pretty()).map_err(|e| e.to_string())
    }
}

/// The error for a `.playlist.ron` version this build has no migrator from.
fn no_playlist_migrator(found: u32) -> String {
    format!("no migrator from playlist.ron v{found} (format_version)")
}

/// `.playlist.ron` (`docs/formats/playlist-ron.md`): the file, one row, and the `order` variants
/// with their payloads (`in_order`, `shuffle(seed)`, `weighted(seed)`; ADR-10064: v1's fingerprint
/// left the `order` enum out).
#[must_use]
pub fn playlist_ron_schema_fingerprint() -> String {
    use regia_rules::playlist::PlaylistOrder;
    let mut order = serde_variant_names::<PlaylistOrder>();
    order.extend(serde_variant_shapes::<PlaylistOrder>());
    join_field_lists(&[
        serde_field_names::<PlaylistFile>(),
        serde_field_names::<regia_rules::playlist::PlaylistEntry>(),
        order,
    ])
}

/// `.playlist.ron` migrates from v1 (the version number only, ADR-10064).
#[must_use]
pub fn playlist_ron_migration_sources() -> Vec<u32> {
    vec![1]
}

/// `regia upgrade-project`: a v1 `.playlist.ron` has its `format_version` rewritten to 2, every
/// other byte kept (v2 has v1's shape, ADR-10064); a v2 file is left as it is. Either way the
/// result must load ([`PlaylistFile::from_ron`]). Any other `format_version` is
/// `no migrator from playlist.ron vN`.
pub fn migrate_playlist_ron(text: &str) -> Result<String, String> {
    #[derive(Deserialize)]
    struct Probe {
        format_version: u32,
    }
    let probe: Probe = ron::from_str(text).map_err(|e| e.to_string())?;
    if !playlist_version_readable(probe.format_version) {
        return Err(no_playlist_migrator(probe.format_version));
    }
    let out = if probe.format_version == PLAYLIST_RON_FORMAT_VERSION {
        text.to_owned()
    } else {
        regia_core::version::rewrite_ron_version(text, "format_version", PLAYLIST_RON_FORMAT_VERSION)
            .ok_or_else(|| "playlist.ron: no top-level `format_version` number".to_owned())?
    };
    PlaylistFile::from_ron(&out)?;
    Ok(out)
}

/// `bans.ron` (`BanList`, `docs/formats/bans-ron.md`).
#[must_use]
pub fn ban_list_schema_fingerprint() -> String {
    use crate::session::bans::{BanKind, BanList, BanListEntry};
    join_field_lists(&[
        serde_field_names::<BanList>(),
        serde_field_names::<BanListEntry>(),
        enum_list::<BanKind>(),
    ])
}

/// `bans.ron` migrates from version 1 (`session::bans::migrate_bans`: a bare IPv6 `Address` entry
/// gains `/128`, ADR-4936); `regia upgrade-project` runs the same step.
#[must_use]
pub fn ban_list_migration_sources() -> Vec<u32> {
    vec![1]
}

/// `net-manifest.ron` (`ContentManifest`, `docs/formats/net-manifest-ron.md`).
#[must_use]
pub fn net_manifest_schema_fingerprint() -> String {
    use crate::session::manifest::{ContentManifest, ManifestMod};
    join_field_lists(&[
        serde_field_names::<ContentManifest>(),
        serde_field_names::<ManifestMod>(),
    ])
}

/// `net-manifest.ron` is at its first version: no migration steps.
#[must_use]
pub fn net_manifest_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// `server.key` (`docs/formats/server-key.md`).
#[must_use]
pub fn server_key_schema_fingerprint() -> String {
    join_field_lists(&[serde_field_names::<crate::crypto::ServerKeyFile>()])
}

/// `server.key` is at its first version: no migration steps.
#[must_use]
pub fn server_key_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// `known_hosts.ron` (`docs/formats/known-hosts-ron.md`, ADR-2416).
#[must_use]
pub fn known_hosts_schema_fingerprint() -> String {
    join_field_lists(&[
        serde_field_names::<crate::known_hosts::KnownHostsFile>(),
        serde_field_names::<crate::known_hosts::KnownHost>(),
        enum_list::<crate::known_hosts::TrustSource>(),
    ])
}

/// `known_hosts.ron` is at its first version: no migration steps.
#[must_use]
pub fn known_hosts_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// `*.siminput.ron` (`SimRecording`, `docs/formats/siminput-ron.md`).
#[must_use]
pub fn sim_recording_schema_fingerprint() -> String {
    join_field_lists(&[serde_field_names::<crate::session::hub::SimRecording>()])
}

/// `*.siminput.ron` is at its first version: no migration steps.
#[must_use]
pub fn sim_recording_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// The version probe and version stamp datagram layouts (`docs/formats/version-probe.md`, spec 120
/// § 12.2, ADR-7985): frozen forever, so the fingerprint is the layout text.
#[must_use]
pub fn version_probe_schema_fingerprint() -> String {
    join_field_lists(&[crate::version_probe::PROBE_LAYOUT
        .iter()
        .map(|line| (*line).to_owned())
        .collect()])
}

/// The probe layout is never migrated: a later probe version keeps version 1's prefix.
#[must_use]
pub fn version_probe_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// `*.desync.ron` (`DesyncBundle`, `docs/formats/desync-ron.md`, spec 120 § 7.6; ADR-8455): the root
/// and every struct and enum it reaches.
#[must_use]
pub fn desync_bundle_schema_fingerprint() -> String {
    use crate::desync::{
        BundleFrame, BundleGame, BundleInput, BundleMembership, BundlePeer, BundleSnapshot, BundleWriter,
        DesyncBundle, DesyncReport, ProbeRows, SubsystemRow,
    };
    join_field_lists(&[
        serde_field_names::<DesyncBundle>(),
        serde_field_names::<BundleGame>(),
        enum_list::<BundleWriter>(),
        serde_field_names::<BundleSnapshot>(),
        serde_field_names::<BundleFrame>(),
        serde_field_names::<BundleInput>(),
        serde_field_names::<BundleMembership>(),
        serde_field_names::<BundlePeer>(),
        enum_list::<crate::lockstep::Voter>(),
        serde_field_names::<ProbeRows>(),
        serde_field_names::<SubsystemRow>(),
        serde_field_names::<DesyncReport>(),
    ])
}

/// `*.desync.ron` is at its first version: no migration steps.
#[must_use]
pub fn desync_bundle_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// The engine's lobby keys (`docs/formats/lobby-keys.md`, spec 89 § 4): every key, the `regia.state`
/// values and the `regia.server` prefixes.
#[must_use]
pub fn lobby_keys_schema_fingerprint() -> String {
    join_field_lists(&[
        crate::lobby::keys::ALL.iter().map(|k| (*k).to_owned()).collect(),
        crate::lobby::keys::STATES
            .iter()
            .map(|k| (*k).to_owned())
            .collect(),
        crate::lobby::keys::SERVER_PREFIXES
            .iter()
            .map(|k| (*k).to_owned())
            .collect(),
    ])
}

/// Layout v1 → v2 (ADR-8897) adds the optional `regia.mode` and `regia.playlist` keys and changes no
/// other key, so v1 data reads unchanged (the step rewrites nothing). Not a file: no `upgrade-project`
/// entry.
#[must_use]
pub fn lobby_keys_migration_sources() -> Vec<u32> {
    vec![1]
}

// Split so `cargo xtask check-packaging` sees a test module (ADR-1485): the same cfg as
// `all(test, feature = "ecs")`.
#[cfg(test)]
#[cfg(feature = "ecs")]
mod tests {
    use super::*;
    use crate::server_host::ServerSettings;
    use regia_rules::playlist::PlaylistOrder;

    /// The probe is frozen forever (spec 120 § 12.2): its layout fingerprint, as
    /// `docs/formats/format-hashes.ron` records it for `version-probe`, never changes.
    #[test]
    fn the_version_probe_fingerprint_is_pinned() {
        let hash = blake3::hash(version_probe_schema_fingerprint().as_bytes())
            .to_hex()
            .to_string();
        assert_eq!(
            hash,
            "917c61de3d18adbdcda018206ea821f501e1fbe700c920e6749fe070199364b2"
        );
        assert!(version_probe_migration_sources().is_empty());
    }

    /// A `server.toml` that sets every listed key to a valid value.
    fn every_key_file() -> String {
        let mut sections: std::collections::BTreeMap<&str, Vec<String>> = Default::default();
        let mut top = String::new();
        for key in SERVER_TOML_KEYS {
            let Some((section, name)) = key.split_once('.') else {
                top.push_str(&format!("{key} = 2\n"));
                continue;
            };
            let value = match *key {
                "server.bind" | "status.bind" => "\"127.0.0.1\"".to_owned(),
                "levels.rotation" => "[\"maps/a.level.ron\"]".to_owned(),
                "levels.match_end_delay_s" => "5.0".to_owned(),
                "net.encryption" => "\"auto\"".to_owned(),
                "status.enabled" => "true".to_owned(),
                "server.port" | "status.port" => "7000".to_owned(),
                "server.tick_hz" | "server.idle_tick_hz" => "30".to_owned(),
                "server.max_players"
                | "net.send_every_ticks"
                | "logs.keep_files"
                | "persistence.keep"
                | "persistence.autosave_ticks" => "2".to_owned(),
                "persistence.enabled"
                | "persistence.save_on_shutdown"
                | "persistence.save_on_level_change" => "true".to_owned(),
                "persistence.player_identity" => "\"platform_or_name\"".to_owned(),
                "levels.start_index" | "net.client_rate_bytes" | "net.server_rate_bytes" => "0".to_owned(),
                "bots.min_players"
                | "bots.max_bots"
                | "bots.join_delay_ticks"
                | "bots.leave_delay_ticks"
                | "bots.seed"
                | "bots.reaction_ticks" => "1".to_owned(),
                "bots.aim_error_radians" | "bots.perception_radius" => "1.0".to_owned(),
                "net.entropy" => "\"off\"".to_owned(),
                "net.interrupt_ms" | "net.timeout_ms" | "net.start_grace_ms" => "30000".to_owned(),
                "net.mtu_probing" => "true".to_owned(),
                "lockstep.enabled" | "lockstep.observer" => "true".to_owned(),
                "lockstep.max_seats" | "lockstep.input_delay_ticks" => "2".to_owned(),
                "lockstep.checkpoint_ticks" | "lockstep.observer_max_lag_ticks" => "30".to_owned(),
                "lockstep.observer_max_backlog_ticks" => "90".to_owned(),
                _ => "\"x\"".to_owned(),
            };
            sections
                .entry(section)
                .or_default()
                .push(format!("{name} = {value}"));
        }
        for (section, lines) in sections {
            top.push_str(&format!("[{section}]\n{}\n", lines.join("\n")));
        }
        top
    }

    #[test]
    fn every_listed_server_toml_key_is_read_without_a_warning() {
        let text = every_key_file();
        let mut settings = ServerSettings::default();
        let (mut errors, mut warnings) = (Vec::new(), Vec::new());
        settings.apply_file(&text, &mut errors, &mut warnings);
        assert!(errors.is_empty(), "{errors:?}\n{text}");
        assert!(warnings.is_empty(), "{warnings:?}\n{text}");
        assert_eq!(settings.status_port, 7000);
        // A key the list does not name is reported, so a parser key renamed away from its listed
        // spelling fails the check above.
        settings.apply_file("[server]\nnot_a_key = 1\n", &mut errors, &mut warnings);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
    }

    #[test]
    fn server_toml_keys_name_every_setting_the_file_holds() {
        // Every `ServerSettings` field the file sets is listed (the console flag and the field
        // names that differ from their key are mapped by hand: `admin_token_file` is
        // `admin.token_file`, `status_*` is `status.*`, `log_dir`/`keep_log_files` are `logs.*`).
        // Optional keys serialize only when set (ADR-7213: the `[net]` rates track presence).
        let value = toml::Value::try_from(ServerSettings {
            entropy: Some(crate::entropy::EntropyMode::Off),
            client_rate_bytes: Some(49_152),
            server_rate_bytes: Some(0),
            send_every_ticks: Some(2),
            ..ServerSettings::default()
        })
        .expect("serializes");
        let fields = value.as_table().expect("a table");
        let mut listed = 0;
        for (field, value) in fields {
            let key = match field.as_str() {
                "console" => continue,
                "timeout" => {
                    for sub in value.as_table().expect("timeout policy").keys() {
                        let key = format!("net.{sub}");
                        assert!(SERVER_TOML_KEYS.contains(&key.as_str()), "{key} is not listed");
                        listed += 1;
                    }
                    continue;
                }
                // `[persistence]` (v2, ADR-3354) is a nested table: one key per field.
                "persistence" => {
                    for sub in value.as_table().expect("a table").keys() {
                        let key = format!("persistence.{sub}");
                        assert!(SERVER_TOML_KEYS.contains(&key.as_str()), "{key} is not listed");
                        listed += 1;
                    }
                    continue;
                }
                "bots" => {
                    let bots = value.as_table().expect("nested bots settings");
                    for bot_field in bots.keys() {
                        listed += 1;
                        let key = format!("bots.{bot_field}");
                        assert!(SERVER_TOML_KEYS.contains(&key.as_str()), "{key} is not listed");
                    }
                    assert_eq!(bots.len(), 8, "a listed bot key has no setting");
                    continue;
                }
                // `[lockstep]` (v4, ADR-8558) is a nested table: one key per field.
                "lockstep" => {
                    let section = value.as_table().expect("nested lockstep settings");
                    for sub in section.keys() {
                        let key = format!("lockstep.{sub}");
                        assert!(SERVER_TOML_KEYS.contains(&key.as_str()), "{key} is not listed");
                        listed += 1;
                    }
                    assert_eq!(
                        section.len(),
                        crate::lockstep::server::LockstepServerSettings::KEYS.len(),
                        "a listed lockstep key has no setting"
                    );
                    continue;
                }
                "admin_token_file" => "admin.token_file".to_owned(),
                "status_enabled" => "status.enabled".to_owned(),
                "status_bind" => "status.bind".to_owned(),
                "status_port" => "status.port".to_owned(),
                "log_dir" => "logs.dir".to_owned(),
                "keep_log_files" => "logs.keep_files".to_owned(),
                "rotation" | "playlist" | "start_index" | "match_end_delay_s" => format!("levels.{field}"),
                "client_rate_bytes" | "server_rate_bytes" | "send_every_ticks" | "encryption"
                | "identity_file" | "entropy" | "mtu_probing" => {
                    format!("net.{field}")
                }
                "format_version" => "format_version".to_owned(),
                other => format!("server.{other}"),
            };
            assert!(
                SERVER_TOML_KEYS.contains(&key.as_str()),
                "{field} -> {key} is not listed"
            );
            listed += 1;
        }
        assert_eq!(listed, SERVER_TOML_KEYS.len(), "a listed key has no setting");
    }

    #[test]
    fn fingerprints_are_complete_and_distinct() {
        let all = [
            net_settings_schema_fingerprint(),
            net_profile_schema_fingerprint(),
            server_toml_schema_fingerprint(),
            ban_list_schema_fingerprint(),
            net_manifest_schema_fingerprint(),
            server_key_schema_fingerprint(),
            sim_recording_schema_fingerprint(),
            lobby_keys_schema_fingerprint(),
            known_hosts_schema_fingerprint(),
            playlist_ron_schema_fingerprint(),
        ];
        for (i, a) in all.iter().enumerate() {
            // No empty list: every listed type really went through serde's struct or enum path.
            assert!(
                !a.split(';').any(str::is_empty),
                "fingerprint {i} has an empty list: {a}"
            );
            for b in &all[i + 1..] {
                assert_ne!(a, b);
            }
        }
        assert!(net_settings_schema_fingerprint().contains("android_listen_max_clients"));
        assert!(net_profile_schema_fingerprint().contains("0:{radius,hysteresis}"));
        assert!(server_key_schema_fingerprint().contains("private_key,public_key"));
        assert!(lobby_keys_schema_fingerprint().contains("regia.protocol"));
        assert!(known_hosts_schema_fingerprint().contains("key,source"));
        assert!(known_hosts_schema_fingerprint().contains("FirstUse"));
        assert!(playlist_ron_schema_fingerprint().contains("map_id,mode,options"));
    }

    /// Every `.playlist.ron` example of the game-modes guide (a ```ron block with `entries`) loads
    /// through the reader the dedicated server uses. regia_framework's rule-set guide test skips
    /// these blocks (integ-v07a, ADR-8895), so this keeps them checked.
    #[test]
    fn the_playlist_in_the_game_modes_guide_loads() {
        let page = include_str!("../../../book/user-guide/game-modes.md");
        let mut loaded = 0;
        for block in page.split("```ron").skip(1) {
            let text = block.split("```").next().unwrap_or_default();
            if !text.contains("entries:") {
                continue;
            }
            let file = PlaylistFile::from_ron(text).unwrap_or_else(|e| panic!("{e}\n{text}"));
            assert!(!file.entries.is_empty(), "{text}");
            loaded += 1;
        }
        assert!(loaded >= 1, "the guide shows a playlist");
    }

    #[test]
    fn playlist_ron_v1_old_fixture_loads() {
        let text = include_str!("../tests/fixtures/formats/playlist-ron/v1.ron");
        let file = PlaylistFile::from_ron(text).expect("v1 loads");
        assert_eq!(file.format_version, PLAYLIST_RON_FORMAT_VERSION);
        assert!(file.looping);
        assert_eq!(file.entries.len(), 2);
        assert_eq!(file.entries[0].mode, "");
        assert_eq!(file.entries[1].mode, "ctf");
        assert_eq!(
            file.entries[1].options.get("limit").map(String::as_str),
            Some("10")
        );
        // v1 -> v2 rewrites the number only (ADR-10064); a second run changes nothing.
        let v2 = text.replacen("format_version: 1", "format_version: 2", 1);
        assert_eq!(migrate_playlist_ron(text).expect("v1 migrates"), v2);
        assert_eq!(migrate_playlist_ron(&v2).expect("v2 stays"), v2);
        let future = text.replacen("format_version: 1", "format_version: 3", 1);
        let err = migrate_playlist_ron(&future).expect_err("a future version");
        assert!(err.contains("no migrator from playlist.ron v3"), "{err}");
        assert!(
            PlaylistFile::from_ron(&future)
                .expect_err("a future version")
                .contains("no migrator from playlist.ron v3")
        );
    }

    /// ADR-10064: the playlist-ron fingerprint covers the `order` enum: its variant names and each
    /// variant's payload fields. v1's fingerprint listed only the file's and the row's fields, so
    /// renaming `shuffle` or its `seed` would not have moved the format hash.
    #[test]
    fn playlist_ron_fingerprint_covers_the_order_variants() {
        let print = playlist_ron_schema_fingerprint();
        assert!(print.contains("in_order,shuffle,weighted"), "{print}");
        assert!(print.contains("1:{seed}"), "{print}");
        assert!(print.contains("2:{seed}"), "{print}");
    }

    /// ADR-10064: an unknown field inside `order` is refused and named, like an unknown field
    /// anywhere else in the file (ADR-8553). v1's reader dropped it, so a typo such as
    /// `shuffle(sed: 4)` was a parse error but `shuffle(seed: 4, cycles: 2)` silently lost `cycles`.
    #[test]
    fn playlist_order_refuses_unknown_fields() {
        let file = |order: &str| {
            format!(
                "(format_version: 2, order: {order}, entries: [(map_id: \"levels/a.level.ron\", mode: \"ctf\")])"
            )
        };
        for (order, field) in [
            ("shuffle(seed: 4, cycles: 2)", "cycles"),
            ("weighted(seed: 4, extra: 1)", "extra"),
        ] {
            let err = PlaylistFile::from_ron(&file(order)).expect_err(order);
            assert!(err.contains(field), "{order}: {err}");
        }
        for (order, want) in [
            ("in_order", PlaylistOrder::InOrder),
            ("shuffle(seed: 4)", PlaylistOrder::Shuffle { seed: 4 }),
            ("weighted(seed: 9)", PlaylistOrder::Weighted { seed: 9 }),
        ] {
            let read = PlaylistFile::from_ron(&file(order)).expect(order);
            assert_eq!(read.order, want, "{order}");
            // What the file writes reads back the same.
            let again = PlaylistFile::from_ron(&read.to_ron().expect("writes")).expect("reads back");
            assert_eq!(again, read, "{order}");
        }
    }

    /// The v2 corpus fixture (every v2 field: `order`, `no_repeat_last`, row weights and player
    /// gates) loads at v2, and its v1 spelling is migrated to the same text.
    #[test]
    fn playlist_ron_v2_fixture_loads() {
        let text = include_str!("../tests/fixtures/formats/playlist-ron/v2.ron");
        let file = PlaylistFile::from_ron(text).expect("v2 loads");
        assert_eq!(file.format_version, 2);
        assert_eq!(file.order, PlaylistOrder::Weighted { seed: 7 });
        assert_eq!(file.no_repeat_last, 1);
        assert_eq!(file.entries.len(), 3);
        assert_eq!(file.entries[1].weight, 3);
        assert_eq!(file.entries[2].min_players, Some(4));
        assert_eq!(file.entries[2].max_players, Some(16));
        assert_eq!(migrate_playlist_ron(text).expect("current"), text);
        let as_v1 = text.replacen("format_version: 2", "format_version: 1", 1);
        assert_eq!(migrate_playlist_ron(&as_v1).expect("v1 spelling"), text);
        assert_eq!(PlaylistFile::from_ron(&as_v1).expect("v1 loads"), file);
    }

    /// One `server.toml` v4 (ADR-8553 folds ADR-8345's `levels.playlist` into the v4 of ADR-7950
    /// and ADR-8300): the frozen 5460-byte v4 fixture with the silence keys still loads, and a
    /// second v4 fixture that sets the playlist path records it (no unknown-key warning).
    #[test]
    fn server_toml_v4_keeps_silence_and_folds_playlist() {
        for key in [
            "net.interrupt_ms",
            "net.timeout_ms",
            "net.start_grace_ms",
            "net.mtu_probing",
            "levels.playlist",
        ] {
            assert!(
                SERVER_TOML_KEYS.contains(&key),
                "{key} missing from the v4 key list"
            );
        }
        let base = include_str!("../tests/fixtures/formats/server-toml/v4.ron");
        assert_eq!(base.len(), 5460, "the frozen v4 fixture is unchanged");
        let settings = ServerSettings::from_toml(base).expect("the frozen v4 fixture loads");
        assert_eq!(settings.format_version, 4);
        assert_eq!(settings.timeout.timeout_ms, 20_000);
        assert_eq!(settings.timeout.interrupt_ms, 1_500);
        assert!(settings.playlist.is_empty());

        let text = include_str!("../tests/fixtures/formats/server-toml/v4-playlist.ron");
        let mut settings = ServerSettings::default();
        let (mut errors, mut warnings) = (Vec::new(), Vec::new());
        settings.apply_file(text, &mut errors, &mut warnings);
        assert!(errors.is_empty(), "{errors:?}");
        assert!(
            warnings.is_empty(),
            "levels.playlist is a v4 key, not ignored: {warnings:?}"
        );
        let settings = ServerSettings::from_toml(text).expect("the v4 playlist fixture loads");
        assert_eq!(settings.format_version, 4);
        assert_eq!(settings.playlist, "rows.playlist.ron");
        assert!(settings.rotation.is_empty());
        assert!(settings.mtu_probing);
        assert_eq!(settings.timeout.timeout_ms, 20_000);

        // One v4 hash: neither pre-fold v4 (silence + MTU only, or playlist only).
        let hash = blake3::hash(server_toml_schema_fingerprint().as_bytes())
            .to_hex()
            .to_string();
        assert_ne!(
            hash,
            "f606b85b45202ff6fb427863d9b13f1330fbd507bc3a736ddab5cecb507164f9"
        );
        assert_ne!(
            hash,
            "cbd7117a18ef4de0d1fb58add3e6a54484d5471c059d9f694eb410856f24bf60"
        );
        assert_eq!(server_toml_migration_sources(), vec![1, 2, 3], "no version 5");
    }

    /// A newer `.playlist.ron` whose body is no longer the current shape still names its version
    /// on the host path (`PlaylistFile::from_ron`), like the migrator. v2 is current since
    /// ADR-10064, so the newer version is 3; a v2 body with a bad shape is a parse error.
    #[test]
    fn playlist_from_ron_names_a_newer_foreign_body() {
        let err = PlaylistFile::from_ron(r#"(format_version: 3, entries: "nope")"#).expect_err("v3");
        assert!(err.contains("no migrator from playlist.ron v3"), "{err}");
        let err = migrate_playlist_ron(r#"(format_version: 3, entries: "nope")"#).expect_err("v3");
        assert!(err.contains("no migrator from playlist.ron v3"), "{err}");
        let err = PlaylistFile::from_ron(r#"(format_version: 2, entries: "nope")"#).expect_err("v2 shape");
        assert!(!err.contains("no migrator"), "{err}");
        // Version 0 is not a version either; a v1 body with a bad shape is a parse error.
        let err = PlaylistFile::from_ron(r#"(format_version: 0, entries: "nope")"#).expect_err("v0");
        assert!(err.contains("v0"), "{err}");
        let err = PlaylistFile::from_ron(r#"(format_version: 1, entries: "nope")"#).expect_err("v1 shape");
        assert!(!err.contains("no migrator"), "{err}");
    }

    /// Unknown fields are refused (ADR-8553): a misspelled `looping` is an error naming the field,
    /// not a silent `false`.
    #[test]
    fn playlist_rejects_a_misspelled_loop_flag() {
        let text = r#"(
            format_version: 1,
            loopng: true,
            entries: [(map_id: "levels/a.level.ron", mode: "", options: {})],
        )"#;
        let err = PlaylistFile::from_ron(text).expect_err("loopng");
        assert!(err.contains("loopng"), "{err}");
        let row = r#"(
            format_version: 1,
            entries: [(map_id: "levels/a.level.ron", mdoe: "ctf", options: {})],
        )"#;
        let err = PlaylistFile::from_ron(row).expect_err("mdoe");
        assert!(err.contains("mdoe"), "{err}");
    }

    /// A row must name a map: an empty `map_id` is refused (review nit).
    #[test]
    fn playlist_rejects_an_empty_map_id() {
        let text = r#"(format_version: 1, entries: [(map_id: "", mode: "", options: {})])"#;
        let err = PlaylistFile::from_ron(text).expect_err("empty map_id");
        assert!(err.contains("map_id"), "{err}");
    }
}
