// SPDX-License-Identifier: MPL-2.0
//! `regia upgrade-project` — run content-format migrators (`docs/specs/12-assets.md` § 4a).
//!
//! Every frozen Regia Engine content format carries a `version` / `format_version` field. `upgrade-project`
//! walks a project's `assets/` (and any other source files), reads each format's version field, and:
//!
//! - **dry run** (default) lists every file with its version and whether it is current,
//! - **`--write`** runs the registered migrator chain for files on an *older* version and rewrites
//!   them in place, keeping a `<file>.bak` of the original.
//!
//! The per-format table below is the registry. A future format change adds a migration step here
//! (and in the loader) at the same time it bumps the format version.
//!
//! The engine pin (spec 94, ADR-1852): the project's `Regia.toml [project] engine` is compared
//! with this engine (`regia_core::engine_version`); a project on a newer engine or outside the
//! support window is refused before anything is written. `--engine <source>` also rewrites every
//! engine dependency of the project to the new source, and `--write` stamps this engine's line in
//! `Regia.toml`; both keep a `.bak` like the migrators. `--json` prints the plan (or the result) as
//! one document (`book/agents/distribution-upgrade.md`). Directories holding an engine (a vendored
//! release or checkout) are never scanned. `--backup <dir>` copies the whole project's source files
//! to `<dir>/<project>-backup-<UTC date>-<time>/` before `--write` changes anything (ADR-7928).

use std::path::{Path, PathBuf};

use clap::Args;
use regia_cli::distribution::{self, EngineSource, PinEdit, Problem, Severity};
use regia_core::engine_version::{self, ReleaseLine};
use serde_json::{Value, json};

/// Upgrade on-disk content formats to the current engine version.
#[derive(Debug, Args)]
pub struct UpgradeProjectArgs {
    /// Project directory.
    #[arg(long, default_value = ".")]
    pub project: PathBuf,
    /// Migrate files in place (a `.bak` of the original is written first). Without this flag the
    /// command only lists what would change.
    #[arg(long)]
    pub write: bool,
    /// Move the project's engine dependencies to this source: `path:<dir>`, `git:<url>#<tag>` or
    /// `registry:<name>[@<version>]` (spec 94 § 3.3). It must name this engine's version.
    #[arg(long)]
    pub engine: Option<String>,
    /// Print the plan (dry run) or the result (`--write`) as one JSON document on stdout.
    #[arg(long)]
    pub json: bool,
    /// With `--write`: copy the project's source files (not `target/`, `build/`, `dist/`, `.git/`,
    /// `.regia/` or caches) to a dated folder under this directory before anything is migrated
    /// (ADR-7928). It must be outside the project.
    #[arg(long, value_name = "DIR")]
    pub backup: Option<PathBuf>,
}

/// How a format stores its version field.
enum VersionSource {
    /// A RON document whose top level is a struct carrying `version` or `format_version`.
    Ron,
    /// A binary header: `magic` (4 bytes) followed by the `u32` LE version.
    Binary(&'static [u8; 4]),
    /// A `version` field that v1 files lack (read as 1 when absent). See [`read_implicit_v1`].
    ImplicitV1,
    /// A reader that also decides whether the file belongs to the format: `Ok(None)` skips it
    /// silently (cooked files of other asset types share the `.fckd` suffix).
    Probe(fn(&[u8]) -> Result<Option<u32>, String>),
}

/// One frozen content format and how to read/migrate it on disk.
struct Format {
    /// Registry name (shown in reports).
    name: &'static str,
    /// File-name suffixes the format owns (e.g. `["level.ron", "prefab.ron"]`). A suffix starting
    /// with `/` names a whole file name (`"/net.ron"` owns `net.ron` but not `cabinet.ron`); one
    /// starting with `~` is a suffix that ignores ASCII case, the rule the cook and the live loader
    /// use for that format (`"~terrain.layers.ron"` owns `terrain.layers.ron` and
    /// `Ground.Terrain.Layers.ron`, ADR-7978).
    suffixes: &'static [&'static str],
    /// The current (latest) version this engine migrates toward.
    current: u32,
    /// Where the version lives on disk.
    source: VersionSource,
    /// Migrates `bytes` from `from` to [`current`](Self::current). Every current v1 format has no
    /// older versions, so this is [`no_migration`]; a future version bump replaces it with a real
    /// step.
    migrate: fn(&[u8], u32) -> Result<Vec<u8>, String>,
}

/// The `regia upgrade-project` migrator of editor format `name` (ADR-7902). Each editor format's
/// own step decides: `editor-layout` v1 -> v2 only added the optional `floating` list (and v2 -> v3
/// the optional `instances` map, ADR-8105 (a)), so the version number is rewritten (the editor reads v1 as it is); a format with no older version has
/// none, and a future bump that changes meaning must add its step here.
fn editor_migrator(name: &str) -> fn(&[u8], u32) -> Result<Vec<u8>, String> {
    match name {
        "editor-layout" => migrate_editor_layout,
        _ => no_migration,
    }
}

/// `editor_layout.ron` / `.editor_layout.ron` v1 -> v2 -> v3: only `format_version` moves (v2 added
/// the optional `floating`; v3 the optional `instances`, and every v1/v2 tab id is instance 0;
/// ADR-8105 (a)).
fn migrate_editor_layout(bytes: &[u8], from: u32) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    if !matches!(from, 1 | 2) {
        return Err(format!("no migrator registered from v{from}"));
    }
    let current = regia_core::editor_formats::editor_format("editor-layout").map_or(3, |f| f.version);
    // Refuse what is not a layout (a `root` is required in every version) before rewriting.
    #[derive(serde::Deserialize)]
    struct Layout {
        root: serde::de::IgnoredAny,
    }
    let serde::de::IgnoredAny = ron::from_str::<Layout>(text)
        .map_err(|e| format!("not a layout: {e}"))?
        .root;
    regia_core::version::rewrite_ron_version(text, "format_version", current)
        .map(String::into_bytes)
        .ok_or_else(|| "no root `format_version` number".to_string())
}

/// `terrain-mesh-layers-cooked` v1 -> v2 (ADR-8872): [`regia_terrain::upgrade_cooked_mesh_layers`].
fn migrate_meshlayers_cooked(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    regia_terrain::upgrade_cooked_mesh_layers(bytes)
}

/// The migrator for a format with no older versions: any `from < current` is a registration gap.
fn no_migration(bytes: &[u8], from: u32) -> Result<Vec<u8>, String> {
    let _ = bytes;
    Err(format!("no migrator registered from v{from}"))
}

/// `.fracture.ron` older than the current version: the chunks gain render meshes (ADR-1967).
/// `.destructible.ron` v1 → v2 (ADR-2734): only the version moves; v2's new fields are optional.
fn migrate_destructible(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| e.to_string())?;
    regia::physics::destruction::migrate_destructible(text)
        .map(String::into_bytes)
        .map_err(|e| format!("{e} ({})", e.code()))
}

fn migrate_fracture(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| e.to_string())?;
    regia_asset_pipeline::fracture::migrate_fracture(text)
        .map(String::into_bytes)
        .map_err(|e| format!("{e} ({})", e.code()))
}

/// `.camera.ron` v1 → v2 (ADR-3231): only the version moves; v2 adds the `Rail` and `Crane`
/// node kinds, and every v1 file means the same in v2.
fn migrate_camera(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| e.to_string())?;
    regia::framework::camera::asset::migrate_camera(text)
        .map(String::into_bytes)
        .map_err(|e| format!("{e} ({})", e.code()))
}

/// `.vehicle.ron` older than the current version: v1 becomes the `Simple` drivetrain (ADR-2598);
/// v1 and v2 wheels get the `Damper` tire (v3, ADR-2719); v1 to v3 slip tires get no transients (v4,
/// ADR-3276).
fn migrate_vehicle(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| e.to_string())?;
    regia::physics::vehicle_asset::migrate_vehicle(text)
        .map(String::into_bytes)
        .map_err(|e| format!("{e} ({})", e.code()))
}

fn migrate_trailer(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| e.to_string())?;
    regia::physics::vehicle_trailer_asset::migrate_trailer(text)
        .map(String::into_bytes)
        .map_err(|e| format!("{e} ({})", e.code()))
}

fn migrate_vehicle_snapshot(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| e.to_string())?;
    regia::physics::vehicle_snapshot::migrate_vehicle_snapshot(text)
        .map(String::into_bytes)
        .map_err(|e| format!("{e} ({})", e.code()))
}

/// `.save` older than the current version: v1 becomes v2 with an empty entity id table; the game
/// converts the entity references on its next load, with its own types (ADR-3239).
fn migrate_save(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    regia::framework::save_entity_refs::upgrade_save_bytes(bytes).map_err(|e| e.to_string())
}

/// Reads the version field of a RON document (first, before any other validation).
///
/// Delegates to [`regia_core::version::peek_ron_version`], which parses the top level as a
/// `ron::Value` map and reads the `version` / `format_version` key, so a file whose body does not
/// match the current struct still reports its version.
fn read_ron_version(bytes: &[u8]) -> Result<u32, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| e.to_string())?;
    regia_core::version::peek_ron_version(text)
}

/// Reads a `u32` LE version from a binary header after checking `magic`.
fn read_binary_version(bytes: &[u8], magic: &'static [u8; 4]) -> Result<u32, String> {
    if bytes.len() < 8 {
        return Err("file is too short to hold a header".to_string());
    }
    if &bytes[..4] != magic {
        return Err(format!(
            "bad magic (expected {}, found {})",
            String::from_utf8_lossy(magic),
            String::from_utf8_lossy(&bytes[..4])
        ));
    }
    Ok(u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]))
}

/// The v1 cue format has no version field (`docs/formats/audio-cue-v1.md`); a file without one is
/// v1, a file with one (v2, ADR-272) reports it.
fn read_implicit_v1(bytes: &[u8]) -> Result<u32, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| e.to_string())?;
    struct Version(u32);
    impl<'de> serde::Deserialize<'de> for Version {
        fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
            struct Head;
            impl<'de> serde::de::Visitor<'de> for Head {
                type Value = Version;

                fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    f.write_str("an audio root struct")
                }

                fn visit_map<M: serde::de::MapAccess<'de>>(self, mut map: M) -> Result<Version, M::Error> {
                    let mut version = None;
                    let mut format_version = None;
                    while let Some(key) = map.next_key::<String>()? {
                        let slot = match key.as_str() {
                            "version" => &mut version,
                            "format_version" => &mut format_version,
                            _ => {
                                map.next_value::<serde::de::IgnoredAny>()?;
                                continue;
                            }
                        };
                        if slot.is_some() {
                            return Err(serde::de::Error::custom(format!("duplicate root {key}")));
                        }
                        *slot = Some(map.next_value::<u32>()?);
                    }
                    Ok(Version(format_version.or(version).unwrap_or(1)))
                }
            }
            // deserialize_any accepts named and bare RON structs without requiring an exact
            // Rust struct name. IgnoredAny consumes valid future enum-valued fields as well.
            de.deserialize_any(Head)
        }
    }
    ron::from_str::<Version>(text)
        .map(|head| head.0)
        .map_err(|err| err.to_string())
}

/// Byte offset of the `(` that opens the root struct, skipping whitespace, `//` and (nested)
/// `/* */` comments and an optional struct name before it. `None` when the root is not a
/// parenthesized struct.
fn root_open_paren(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => return Some(i),
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                let mut depth = 1;
                i += 2;
                while i < bytes.len() && depth > 0 {
                    if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                        depth += 1;
                        i += 2;
                    } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
            }
            b if b.is_ascii_whitespace() || b.is_ascii_alphanumeric() || b == b'_' => i += 1,
            _ => return None,
        }
    }
    None
}

/// `.cue.ron` v1 and v2 → v3: every field a newer version added is optional, so the migration only
/// stamps the version and keeps the rest of the file (comments included) byte for byte. A v1 file
/// has no `version` field and gets one after the opening parenthesis; a v2 file already has one,
/// whose number is rewritten in place (ADR-272, ADR-1697).
fn migrate_cue(bytes: &[u8], from: u32) -> Result<Vec<u8>, String> {
    stamp_implicit_v1(bytes, from, regia::audio::CUE_VERSION, "cue")
}

/// `.music.ron` v1 → v2: `segment_bars`, `stingers` and `transitions` are optional, so the migration
/// only stamps the version (text-preserving, comments kept).
fn migrate_music(bytes: &[u8], from: u32) -> Result<Vec<u8>, String> {
    stamp_implicit_v1(bytes, from, regia::audio::MUSIC_VERSION, "music")
}

/// Stamps `current` on a RON file whose v1 has no `version` field: rewrites the number when it has
/// one, else inserts the field after the root's opening parenthesis.
fn stamp_implicit_v1(bytes: &[u8], from: u32, current: u32, what: &str) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    if read_implicit_v1(bytes)? != from {
        return Err(format!("{what} version changed after planning; nothing written"));
    }
    let open = root_open_paren(text).ok_or_else(|| format!("{what} file has no opening parenthesis"))?;
    let out = if let Some(rewritten) = regia_core::version::rewrite_ron_version(text, "version", current) {
        rewritten
    } else {
        if from >= 2 {
            return Err(format!(
                "{what} v{from} has no safely rewritable root version field"
            ));
        }
        let mut out = String::with_capacity(text.len() + 16);
        out.push_str(&text[..=open]);
        out.push_str(&format!("\n    version: {current},"));
        out.push_str(&text[open + 1..]);
        out
    };
    if read_implicit_v1(out.as_bytes())? != current {
        return Err(format!("{what} root version was not stamped; nothing written"));
    }
    Ok(out.into_bytes())
}

/// `<map>.holes.ron` (terrain holes): a `version` field when present, else v1 (a bare
/// serialised `HolesMask` is version 1, `regia_terrain::HolesMask::from_holes_ron`).
fn holes_file_version(bytes: &[u8]) -> Result<Option<u32>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| e.to_string())?;
    // An unreadable file is an error, never "version 1 (current)".
    regia_terrain::holes_file_version(text)
        .map(Some)
        .map_err(|e| e.to_string())
}

/// `heightfield.raster.json` (a count raster's layout, ADR-8542): the `raster_descriptor` key, read
/// before the typed parse, so a newer file reports its version instead of an unknown field (ADR-8639).
fn terrain_raster_descriptor_version(bytes: &[u8]) -> Result<Option<u32>, String> {
    regia_terrain::source::raster_descriptor_file_version(bytes)
        .map(Some)
        .map_err(|e| e.to_string())
}

/// `heightfield.tiles.json` (a heightfield in several files, ADR-8595): the `terrain_source_tiles`
/// key, read before the typed parse (ADR-8639).
fn terrain_source_tiles_version(bytes: &[u8]) -> Result<Option<u32>, String> {
    regia_terrain::source::tile_set_file_version(bytes)
        .map(Some)
        .map_err(|e| e.to_string())
}

/// `manifest-<platform>.json.sig` (ADR-8846): the `format` key. A `.json.sig` file that is
/// not a release signature (another tool's) is skipped, not reported.
fn release_signature_version(bytes: &[u8]) -> Result<Option<u32>, String> {
    Ok(regia_release_chunks::peek_signature_version(bytes).ok())
}

/// `<map>.viscells.ron` (indoor cells and portals): a `version` field when present, else v1
/// (`regia_render::viscells::viscells_file_version`).
fn viscells_file_version(bytes: &[u8]) -> Result<Option<u32>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| e.to_string())?;
    // An unreadable file is an error, never "version 1 (current)".
    regia_render::viscells::viscells_file_version(text)
        .map(Some)
        .map_err(|e| e.to_string())
}

/// Cooked meshes (`.fckd` with a `CookedMesh` payload): v1 loads with no skin, colour or morph
/// blocks, and v1 and v2 load with four skin influences (`regia_asset::CookedMesh::from_postcard`,
/// ADR-1622), so upgrading is decode → re-encode at the current version with the cooked header
/// kept. Paks are not rewritten: the loader reads older entries in place.
fn migrate_cooked_mesh(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    regia_asset::migrate_cooked_mesh_file(bytes)?.ok_or_else(|| "not an older cooked mesh".to_string())
}

/// Cooked voxel far proxies (`.fckd` with an `FVOX` payload): v1 (whole cubes) loads with every
/// sub-voxel box full, so upgrading is decode → re-encode at v2 with the cooked header kept
/// (ADR-2655).
fn migrate_voxel_far(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    regia::render::visbuffer::migrate_cooked_voxel_far_file(bytes)?
        .ok_or_else(|| "not an older voxel far proxy".to_string())
}

/// Cooked clips (`.fckd` with a clip payload): v1 loads with every morph track unbound, so upgrading
/// is decode → re-encode at v2 with the cooked header kept.
fn migrate_cooked_clip(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    regia_animation::cooked::migrate_cooked_clip_file(bytes)
}

/// Cooked terrain index (`regia_terrain::stream`): v1 -> v2 adds empty heights, holes and material
/// (ADR-8024) and `source_mapping: None` (ADR-8471), one v2 (ADR-8619); decode -> re-encode with the
/// cooked header kept.
fn migrate_terrain_index(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    regia_terrain::stream::migrate_terrain_index_file(bytes)?
        .ok_or_else(|| "not an older terrain index".to_string())
}

/// `.flmap` lightmaps: v1 → v2 adds the empty direction section (ADR-1556), v2 → v3 entries with no
/// key and no seam figures (ADR-2024), v3 → v4 the empty cluster section (ADR-2107), v4 → v5 an
/// empty denoise record with the texels kept RGB9E5 (ADR-2314), v5 → v6 no source hashes
/// (ADR-2656), v6 → v7 one atlas page (every entry on page 0, ADR-3300); texels, coverage and
/// geometry are copied unchanged.
fn migrate_lightmap(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    regia_asset::lightmap::migrate_lightmap_file(bytes)
}

/// `.pgout`: v1 (one untiled cell, no tier or cell hash), v2 (sections without meshes, terrain
/// edits or road decals) and v3 (no set layers) are rewritten as v4 with the same header and
/// content (`regia_procgen::pgout::migrate`). Their prefab records have no overrides, so the result
/// has no override section (ADR-7750); a v4 file, with or without one, comes back unchanged.
fn migrate_pgout(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    regia_procgen::pgout::migrate(bytes).map_err(|e| e.to_string())
}

/// `<map>.terrain.editlayers` v1 → v2 (ADR-8381): heights stay bit-identical; weights and the
/// procgen target are absent.
fn migrate_edit_layers(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    regia_terrain::migrate_edit_layers_bytes(bytes).map_err(|e| e.to_string())
}

/// `.material.ron`: the loader runs the v1 → v2 chain, so upgrading is load → re-save.
fn migrate_material(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    regia_asset::migrate_material_ron_bytes(bytes).map_err(|e| e.to_string())
}

/// Terrain layer files: the loader runs the registered migrator chain (`regia_terrain::layers`),
/// so upgrading is load → re-save at the current version.
fn migrate_terrain_layers(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    let layers = regia_terrain::TerrainLayers::from_ron(text).map_err(|e| e.to_string())?;
    let out = layers.to_ron().map_err(|e| e.to_string())?;
    Ok(out.into_bytes())
}

/// `.audiopatch.ron` v1 → v2 (ADR-7226): every v2 field is optional, so the file is checked by the
/// loader (which runs the migrator chain and refuses a v1 file that already uses a v2 field) and
/// then only its version number is rewritten, comments and layout kept.
fn migrate_patch(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    regia::audio::AudioPatchDef::from_ron(text).map_err(|e| e.to_string())?;
    bump_ron_version(text, regia::audio::PATCH_VERSION)
}

/// `.shots.ron`: v1 → v2 only adds the optional `rotation` track and v2 → v3 only allows path and
/// look-at keys past a shot's end (ADR-8640), so upgrading is load (the loader runs the chain) →
/// re-save at the current version.
fn migrate_shots(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    let sequence = regia::framework::shots::ShotSequence::from_ron(text).map_err(|e| e.to_string())?;
    sequence
        .to_ron()
        .map(String::into_bytes)
        .map_err(|e| e.to_string())
}

/// `.meta` v1 → v2 (ADR-7819): the loader's step turns the `lossless_clips` setting off. A file
/// whose settings the step leaves as they were only needs its version rewritten (comments and
/// layout kept); one whose settings named the key is written back from the migrated sidecar.
fn migrate_meta(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    let opts = ron::Options::default().with_default_extension(ron::extensions::Extensions::IMPLICIT_SOME);
    let raw: regia_asset::MetaSidecar = opts.from_str(text).map_err(|e| e.to_string())?;
    let meta = regia_asset::MetaSidecar::from_ron(bytes).map_err(|e| e.to_string())?;
    if meta.settings == raw.settings {
        regia_core::version::rewrite_ron_version(text, "format_version", meta.format_version)
            .map(String::into_bytes)
            .ok_or_else(|| "no root `format_version` number".to_string())
    } else {
        meta.to_ron_string()
            .map(String::into_bytes)
            .map_err(|e| e.to_string())
    }
}

/// `text` with its `version` set to `current`, the rest byte for byte (hand-written files keep
/// their comments).
fn bump_ron_version(text: &str, current: u32) -> Result<Vec<u8>, String> {
    regia_core::version::rewrite_ron_version(text, "version", current)
        .map(String::into_bytes)
        .ok_or_else(|| "no root `version` number".to_string())
}

/// `.tilemap.ron`: v1 → v2 only adds `Orientation` variants (ADR-2597), so a file that loads only
/// needs its version number rewritten (comments and layout kept).
fn migrate_tilemap(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    let map = regia::two_d::tilemap::TileMapAsset::from_ron(text).map_err(|e| e.to_string())?;
    bump_ron_version(text, map.version)
}

/// `.autotile.ron`: v1 → v2 only adds the `WangCorner` layout (ADR-2597); same rewrite.
fn migrate_autotile(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    let set = regia::two_d::autotile::AutoTileSet::from_ron(text).map_err(|e| e.to_string())?;
    bump_ron_version(text, set.version)
}

/// `.seq.ron`: v1 → v2 only adds variants and optional fields (Bezier tangents, clip, audio and
/// sub-sequence tracks, ADR-1880), and v2 → v3 only optional fields (spatial sound sections, the
/// voice time scale, looping sub-sequences, ADR-2405), v3 → v4 only a variant (video sections,
/// ADR-2862), v4 → v5 only optional root fields (spawnables, `restore_on_stop`, ADR-2948), and
/// v5 → v6 only a variant and an optional field (event tracks, `locked`, ADR-8105), so a file that
/// loads only needs its version number rewritten.
fn migrate_sequence(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    use regia::framework::sequencer::Sequence;
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    let sequence = Sequence::from_ron(text).map_err(|e| e.to_string())?;
    let bumped = bump_ron_version(text, sequence.version)?;
    // Most steps only add fields whose defaults are the old behavior, so the file keeps every
    // byte but its version. The v4 → v5 step (ADR-3197) writes `doppler: 0.0` on spatial
    // sections (Doppler is on by default in v5): when the bumped text would read differently from
    // the migrated sequence, the migrated sequence is saved instead.
    let reread = std::str::from_utf8(&bumped)
        .map_err(|e| e.to_string())
        .and_then(|t| Sequence::from_ron(t).map_err(|e| e.to_string()))?;
    if reread == sequence {
        Ok(bumped)
    } else {
        sequence
            .to_ron()
            .map(String::into_bytes)
            .map_err(|e| e.to_string())
    }
}

/// `.navlanes.ron`: v1 → v2 only adds the optional `weight` and `opposite` (ADR-2133): a file that
/// loads only needs its version number rewritten.
fn migrate_navlanes(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    regia_nav::migrate_nav_lanes(text)
        .map(String::into_bytes)
        .map_err(|e| e.to_string())
}

/// `config/net.ron`: v1 → v2 adds optional entropy settings (ADR-4854).
fn migrate_net_settings(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    regia_net::settings::migrate_net_settings(text)
        .map(String::into_bytes)
        .map_err(|e| e.to_string())
}

/// `.netprofile.ron`: v1 → v2 only adds the optional `rate_tiers` (ADR-8031) and
/// `extrapolation_gate_m` (ADR-8522), so the step rewrites the version number.
fn migrate_net_profile(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    regia_net::profile::NetProfile::migrate_ron(text)
        .map(String::into_bytes)
        .map_err(|e| e.to_string())
}

/// `server.toml`: v1/v2 → v3 adds optional persistence, bots and entropy (ADR-4854);
/// v3 → v4 adds optional `levels.playlist` (ADR-8345). The step rewrites the version line.
fn migrate_server_toml(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    regia_net::server_host::config::migrate_server_toml(text).map(String::into_bytes)
}

/// `.rules.ron` v1 has no older file (ADR-8694). A different `format_version` is refused.
fn migrate_rules_ron(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    regia::framework::ruleset_file::migrate_rules_ron(text).map(String::into_bytes)
}

/// `.playlist.ron` v1 -> v2 rewrites the version number and keeps every other byte (the shape did
/// not change; v2's reader refuses unknown fields inside `order`, ADR-10064). Any other
/// `format_version` is refused.
fn migrate_playlist_ron(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    regia_net::formats::migrate_playlist_ron(text).map(String::into_bytes)
}

/// `bans.ron`: v1 → v2 rewrites every bare IPv6 `Address` entry to its `/128` form, preserving
/// the exact-address meaning v1 defined (ADR-4936).
fn migrate_bans(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    regia_net::session::bans::migrate_bans(text)
        .map(String::into_bytes)
        .map_err(|e| e.to_string())
}

/// `.render_job.ron`: v1 → v2 only adds the optional `bindings` table (ADR-1880), v2 → v3 the
/// optional `output.encode` (ADR-3369) and v3 → v4 the optional `product` block (ADR-2757): a file
/// that loads only needs its version number rewritten.
fn migrate_render_job(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    let job = regia_agent::render_job::RenderJob::from_ron(text).map_err(|e| e.to_string())?;
    bump_ron_version(text, job.version)
}

/// `.scatter.ron`: v1 (one rule) loads as a one-class v2 set (`ScatterSet::from_v1`); upgrading
/// re-saves it at v2.
fn migrate_scatter(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    let set = regia_terrain::ScatterSet::from_ron(text, None).map_err(|e| e.to_string())?;
    let out = set.to_ron().map_err(|e| e.to_string())?;
    Ok(out.into_bytes())
}

/// `<map>.holes.ron`: v1 → v2 only adds the optional named holes (ADR-1595), so upgrading is
/// load (`HolesMask::from_holes_ron` reads v1) → re-save at v2.
fn migrate_holes(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    let mask = regia_terrain::HolesMask::from_holes_ron(text).map_err(|e| e.to_string())?;
    let out = mask.to_holes_ron().map_err(|e| e.to_string())?;
    Ok(out.into_bytes())
}

/// `.vfx.ron`: the loader runs the v1 → v8 chain (`VfxSystemDef::from_ron`), so upgrading is
/// load, then re-save at the current version.
fn migrate_vfx(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    let def = regia_vfx::VfxSystemDef::from_ron(text).map_err(|e| e.to_string())?;
    def.to_ron().map(String::into_bytes).map_err(|e| e.to_string())
}

/// `.matgraph.ron`: v1 → v2 (every v2 field is optional), v2 → v3 (adds the `Wind` node),
/// v3 → v4 (adds the `Noise` node) and v4 → v5 (adds the `Code` node, ADR-7852) only move the
/// version, so upgrading is load (the loader runs the chain) → re-save at the current version.
fn migrate_matgraph(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    let graph = regia_matgraph::MaterialGraph::from_ron(text).map_err(|e| e.to_string())?;
    let out = graph.to_ron().map_err(|e| e.to_string())?;
    Ok(out.into_bytes())
}

/// `.matfunc.ron`: v1 → v2 (adds the `Wind` node kind to the node set), v2 → v3 (adds the
/// `Noise` node, ADR-2318) and v3 → v4 (adds the `Code` node, ADR-7852) only move the version,
/// so upgrading is load (the loader runs the chain) → re-save at the current version.
fn migrate_matfunc(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    let function = regia_matgraph::MaterialFunction::from_ron(text).map_err(|e| e.to_string())?;
    let out = function.to_ron().map_err(|e| e.to_string())?;
    Ok(out.into_bytes())
}

/// `.matinst.ron`: v1 → v2 only moves the version (`switches` is optional).
fn migrate_matinst(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    let instance = regia_matgraph::MaterialInstance::from_ron(text).map_err(|e| e.to_string())?;
    let out = instance.to_ron().map_err(|e| e.to_string())?;
    Ok(out.into_bytes())
}

/// `.animgraph.ron`: the loader runs the registered chain (v1 → v2 adds inertialization
/// transitions), so upgrading is load → re-save at the current version.
fn migrate_animgraph(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    let graph = regia_animation::AnimGraph::from_ron_str(text).map_err(|e| e.to_string())?;
    let out = graph.to_ron_string().map_err(|e| e.to_string())?;
    Ok(out.into_bytes())
}

/// `.montage.ron`: v1 → v2 only adds fields with defaults (warp window modes and anchors, the
/// rate curve, segment links; spec 34, ADR-1645), so upgrading is load → re-save at v2.
fn migrate_montage(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    let montage = regia_animation::Montage::from_ron_str(text).map_err(|e| e.to_string())?;
    let out = montage.to_ron_string().map_err(|e| e.to_string())?;
    Ok(out.into_bytes())
}

/// Scene files (`.level.ron`, `.prefab.ron`): the loader runs the registered chain
/// (`regia_scene::format`, v1 → v2 adds the level atmosphere), so upgrading is load →
/// re-save at the current version.
fn migrate_scene(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    let file = regia_scene::format::SceneFile::from_ron_str(text).map_err(|e| e.to_string())?;
    let out = file.to_ron_string().map_err(|e| e.to_string())?;
    Ok(out.into_bytes())
}

/// `.variant.ron`: a missing `format_version` reads as 0 (hand-written v1 shape).
fn read_prefab_variant_version(bytes: &[u8]) -> Result<Option<u32>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| e.to_string())?;
    Ok(Some(regia_core::version::peek_ron_version(text).unwrap_or(0)))
}

/// `.variant.ron` v0 to v1 (ADR-2323): load (the loader migrates), then re-save.
fn migrate_prefab_variant(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    let file =
        regia_scene::prefab_variant::PrefabVariantFile::from_ron_str(text).map_err(|e| e.to_string())?;
    let out = file.to_ron_string().map_err(|e| e.to_string())?;
    Ok(out.into_bytes())
}

/// `.rprof.json`: the `format_version` of the JSON document, 0 when it is left out.
fn read_profile_capture_version(bytes: &[u8]) -> Result<Option<u32>, String> {
    let value: Value = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    let found = value.get("format_version").and_then(Value::as_u64).unwrap_or(0);
    Ok(Some(u32::try_from(found).unwrap_or(u32::MAX)))
}

/// `.rprof.json` v0 to v1 (ADR-2323) and v1 to v2 (ADR-7325, the optional per-thread timeline):
/// the reader reads both older shapes as the current one (no timeline), so upgrading is read, then
/// write at the current version.
fn migrate_profile_capture(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    let capture =
        regia_telemetry::profile_capture::ProfileCapture::from_json(text).map_err(|e| e.to_string())?;
    capture
        .to_json()
        .map(String::into_bytes)
        .map_err(|e| e.to_string())
}

/// `.theme.ron`: v0 files are the pre-v1 `UiTheme` serde shape and carry no version field, so a
/// missing version reads as 0 (an unparsable file reads as 0 too and fails in the migrator with
/// the parser message).
fn read_theme_version(bytes: &[u8]) -> Result<Option<u32>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| e.to_string())?;
    Ok(Some(regia_core::version::peek_ron_version(text).unwrap_or(0)))
}

/// `config/audio.ron` (`audio-buses`): the stored `version` (0 when absent); `None` for an
/// `audio.ron` that is not a bus layout.
fn read_audio_buses_version(bytes: &[u8]) -> Result<Option<u32>, String> {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Ok(None);
    };
    Ok(regia::audio::BusLayout::from_ron(text)
        .ok()
        .map(|layout| layout.version))
}

/// `config/audio.ron` v0 → v1 (ADR-8354): inserts `version: 1` and keeps every other byte
/// (`regia_audio::migrate_audio_buses_ron`).
fn migrate_audio_buses(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    regia::audio::migrate_audio_buses_ron(text).map(String::into_bytes)
}

/// `.theme.ron` v0 to the current version: the loader runs the v0 migrator
/// (`regia_ui::theme_migrate`), so upgrading is load, then re-save at the current version.
/// v1 to v2 only adds fields (ADR-8105 (e)): the file is checked by the loader and only its
/// `format_version` number is rewritten, every other byte kept.
fn migrate_theme(bytes: &[u8], from: u32) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    let file = regia_ui::parse_theme_ron(text, "theme.ron").map_err(|e| e.to_string())?;
    if from >= 1 {
        return regia_core::version::rewrite_ron_version(
            text,
            "format_version",
            regia_ui::THEME_FORMAT_VERSION,
        )
        .map(String::into_bytes)
        .ok_or_else(|| "theme file has no top-level `format_version`".to_owned());
    }
    let out = file.to_ron_string().map_err(|e| e.to_string())?;
    Ok(out.into_bytes())
}

/// `.ui.ron` v1 → v2 (ADR-1381): v2 only adds the menu widgets, so upgrading rewrites the root
/// `version` number and keeps the rest of the file byte for byte (`regia_ui::upgrade_ui_ron`).
fn migrate_ui(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    regia_ui::upgrade_ui_ron(text, "ui.ron")
        .map(String::into_bytes)
        .map_err(|e| e.to_string())
}

/// `.uisettings.ron` v1 → v2 (ADR-8252): v2 adds the optional `scale_mode`. Upgrading rewrites
/// only the root `version` number and keeps every other byte.
fn migrate_uisettings(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    regia_ui::migrate_ui_settings_ron(text).map(String::into_bytes)
}

/// `.bake.ron` (GI bake descriptions): v0 files have no `format_version` (read as 0).
fn read_bake_version(bytes: &[u8]) -> Result<Option<u32>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| e.to_string())?;
    Ok(Some(regia_pt::bake_desc::bake_desc_version(text)))
}

/// `.bake.ron` v0 → v1 → v2 → v3: stamps `format_version` (v0) or rewrites its number (v1, v2),
/// turns a v0-v2 root `sun: Some(s)` into `suns: [s]` (v3, ADR-7818) and keeps the rest of the file
/// byte for byte (`regia_pt::bake_desc::migrate_bake_ron`).
fn migrate_bake(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    regia_pt::bake_desc::migrate_bake_ron(text)
        .map(String::into_bytes)
        .map_err(|e| e.to_string())
}

/// `.logic.ron` v1 → v2 (ADR-8900): v2 only adds the optional `entity_vars` and `world_vars`, so the
/// file is checked by the loader and only its root `format_version` number is rewritten, every
/// other byte kept.
fn migrate_logic(bytes: &[u8], from: u32) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    if !regia_ai::logic::logic_migration_sources().contains(&from) {
        return Err(format!("no migrator registered from .logic.ron v{from}"));
    }
    regia_ai::LogicGraphDef::from_ron(text).map_err(|e| e.to_string())?;
    regia_core::version::rewrite_ron_version(text, "format_version", regia_ai::logic::LOGIC_FORMAT_VERSION)
        .map(String::into_bytes)
        .ok_or_else(|| "logic graph has no top-level `format_version`".to_owned())
}

/// `.uianim.ron` v1 → v2 (ADR-8900): v2 only adds the optional frame `events`. The file is checked
/// by the loader; a root `format_version` has its number rewritten (every other byte kept), and a
/// v1 file without the field (read as v1) is re-saved by the loader at v2.
fn migrate_uianim(bytes: &[u8], from: u32) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    if !regia_ui::anim_timeline::uianim_migration_sources().contains(&from) {
        return Err(format!("no migrator registered from .uianim.ron v{from}"));
    }
    let lib = regia_ui::anim_timeline::UiAnimLibrary::from_ron(text).map_err(|e| e.to_string())?;
    match regia_core::version::rewrite_ron_version(
        text,
        "format_version",
        regia_ui::anim_timeline::UIANIM_FORMAT_VERSION,
    ) {
        Some(out) => Ok(out.into_bytes()),
        None => lib
            .to_ron_string()
            .map(String::into_bytes)
            .map_err(|e| e.to_string()),
    }
}

/// `.statetree.ron` v1 → v2: v2 only adds optional fields, so upgrading rewrites the root
/// `format_version` number and keeps the rest of the file byte for byte
/// (`regia_ai::state_tree::migrate_ron_v1_to_v2`, spec 38).
fn migrate_statetree(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    regia_ai::state_tree::migrate_ron_v1_to_v2(text)
        .map(String::into_bytes)
        .map_err(|e| e.to_string())
}

/// `.bt.ron` v1 → v2: inserts or rewrites the root `format_version` and keeps every other byte
/// (`regia_ai::migrate_bt_ron`, ADR-4543).
fn migrate_bt(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    regia_ai::migrate_bt_ron(text).map(String::into_bytes)
}

/// `.spatialquery.ron` v1 → v2: v2 only adds the `Custom` generator and test, so upgrading rewrites the
/// root `format_version` number and keeps the rest of the file byte for byte
/// (`regia_ai::spatial_query::migrate_ron_v1_to_v2`, ADR-2709).
fn migrate_spatialquery(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    regia_ai::spatial_query::migrate_ron_v1_to_v2(text)
        .map(String::into_bytes)
        .map_err(|e| e.to_string())
}

/// Whether registry suffix `suffix` owns file name `name`: `/name` is a whole file name, `~suffix` a
/// suffix that ignores ASCII case (ADR-7978), anything else a suffix.
fn owns_name(suffix: &str, name: &str) -> bool {
    if let Some(exact) = suffix.strip_prefix('/') {
        return name == exact;
    }
    if let Some(any_case) = suffix.strip_prefix('~') {
        let (n, s) = (name.as_bytes(), any_case.as_bytes());
        return n.len() >= s.len() && n[n.len() - s.len()..].eq_ignore_ascii_case(s);
    }
    name.ends_with(suffix)
}

/// The first postcard field of `bytes` as a `u32` (an unsigned LEB128 varint of at most five bytes):
/// the `format_version` of the postcard files that store it first (`.convc`, `<map>.weights.bin`).
fn postcard_leading_u32(bytes: &[u8]) -> Result<u32, String> {
    let mut value: u64 = 0;
    for (i, byte) in bytes.iter().take(5).enumerate() {
        value |= u64::from(byte & 0x7f) << (7 * i);
        if byte & 0x80 == 0 {
            return u32::try_from(value).map_err(|_| "the leading version does not fit a u32".to_string());
        }
    }
    Err("the file does not start with a postcard version number".to_string())
}

/// `hub.toml`: the top-level `format_version` (absent reads as 1, as the hub does).
fn hub_toml_version(bytes: &[u8]) -> Result<Option<u32>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    let table: toml::Table = text
        .parse()
        .map_err(|e: toml::de::Error| e.message().to_string())?;
    match table.get("format_version") {
        None => Ok(Some(1)),
        Some(value) => value
            .as_integer()
            .and_then(|v| u32::try_from(v).ok())
            .map(Some)
            .ok_or_else(|| "format_version is not a non-negative integer".to_string()),
    }
}

/// `server.toml`: the top-level `format_version` (absent reads as 1, as the server does).
fn server_toml_version(bytes: &[u8]) -> Result<Option<u32>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    let table: toml::Table = text
        .parse()
        .map_err(|e: toml::de::Error| e.message().to_string())?;
    match table.get("format_version") {
        None => Ok(Some(1)),
        Some(value) => value
            .as_integer()
            .and_then(|v| u32::try_from(v).ok())
            .map(Some)
            .ok_or_else(|| "format_version is not a non-negative integer".to_string()),
    }
}

/// `permutations.json`: the root `format_version`.
fn permutations_version(bytes: &[u8]) -> Result<Option<u32>, String> {
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    value
        .get("format_version")
        .and_then(serde_json::Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .map(Some)
        .ok_or_else(|| "no root `format_version` number".to_string())
}

/// `asset_registry.bin` / `<pak>.registry.bin`: the 8-byte magic, then the `u32` LE version.
fn asset_registry_version(bytes: &[u8]) -> Result<Option<u32>, String> {
    if bytes.len() < 12 || bytes[..8] != regia_asset::registry::REGISTRY_MAGIC {
        return Err("bad magic (expected an asset registry file)".to_string());
    }
    let mut version = [0u8; 4];
    version.copy_from_slice(&bytes[8..12]);
    Ok(Some(u32::from_le_bytes(version)))
}

/// `<map>.meshlayers.ron` and `<map>.passages.ron`: the root `version` (read alone, so a newer file
/// with unknown fields still reports its version).
fn meshlayers_version(bytes: &[u8]) -> Result<Option<u32>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    regia_world_geom::meshlayers::peek_version(text)
        .map(Some)
        .map_err(|e| e.to_string())
}

/// `.convc` and `<map>.weights.bin`: postcard with the version first.
fn postcard_version(bytes: &[u8]) -> Result<Option<u32>, String> {
    postcard_leading_u32(bytes).map(Some)
}

/// `.table.ron`: the envelope's `format_version`; a bare `[...]` row list is v0.
fn data_table_version(bytes: &[u8]) -> Result<Option<u32>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    if let Ok(version) = regia_core::version::peek_ron_version(text) {
        return Ok(Some(version));
    }
    regia_asset::data_table::RawTable::from_ron(text)
        .map(|table| Some(table.source_version))
        .map_err(|issue| issue.to_string())
}

/// `.table.ron` v0 (a bare row list) → v1: read through the loader's migration chain, write the
/// v1 envelope.
fn migrate_data_table(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    let raw = regia_asset::data_table::read_raw_ron(text).map_err(|issue| issue.to_string())?;
    Ok(raw.to_ron().into_bytes())
}

/// `.session.ron` v1 → v2 only adds the optional role field `remote` (ADR-2137): a v1 file that
/// parses only needs its version number rewritten.
fn migrate_session(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    regia_cli::session::Session::parse(text).map_err(|e| e.to_string())?;
    regia_core::version::rewrite_ron_version(text, "format_version", regia_cli::session::SESSION_FILE_VERSION)
        .map(String::into_bytes)
        .ok_or_else(|| "no root `format_version` number".to_string())
}

/// `input_overrides.ron` v1 (no `format_version`, no `slots`) is a subset of v2: read (the reader
/// accepts v1) and write back at v2.
fn migrate_input_overrides(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    let overrides = regia_input::InputOverrides::from_ron(text)?;
    overrides
        .try_to_ron()
        .map(String::into_bytes)
        .map_err(|e| e.to_string())
}

/// A format with no older versions yet. `current` is the format crate's own constant, never a
/// literal: a bump that forgets to replace [`no_migration`] here then plans old files as
/// `v1 -> v2` and fails `every_format_with_older_versions_has_a_migrator` (ADR-1378).
fn unmigrated(
    name: &'static str,
    suffixes: &'static [&'static str],
    current: u32,
    source: VersionSource,
) -> Format {
    Format {
        name,
        suffixes,
        current,
        source,
        migrate: no_migration,
    }
}

/// The registry of every frozen format. A version bump adds a step to the loader and replaces the
/// format's [`no_migration`] with the real migrator here.
fn formats() -> Vec<Format> {
    let mut formats = vec![
        // v2 adds the `lossless_clips` import setting; v1 migrates with it off (ADR-7819).
        Format {
            name: "meta",
            suffixes: &[".meta"],
            current: regia_asset::META_FORMAT_VERSION,
            source: VersionSource::Ron,
            migrate: migrate_meta,
        },
        Format {
            name: "material",
            suffixes: &[".material.ron"],
            current: regia_asset::MATERIAL_FORMAT_VERSION,
            source: VersionSource::Ron,
            migrate: migrate_material,
        },
        Format {
            name: "scene-level",
            suffixes: &[".level.ron"],
            current: regia_scene::format::SCENE_FORMAT_VERSION,
            source: VersionSource::Ron,
            migrate: migrate_scene,
        },
        Format {
            name: "scene-prefab",
            suffixes: &[".prefab.ron"],
            current: regia_scene::format::SCENE_FORMAT_VERSION,
            source: VersionSource::Ron,
            migrate: migrate_scene,
        },
        // Runtime UI layouts: v2 adds the menu widgets; v1 → v2 renumbers (ADR-1381). Every later step
        // renumbers too (v8: components, ADR-3147; v9: pictures named by asset path or icon, ADR-3148;
        // v10: game widgets, ADR-2958; v11: text input options and image fit, ADR-9980).
        Format {
            name: "ui",
            suffixes: &[".ui.ron"],
            current: regia_ui::UI_RON_VERSION,
            source: VersionSource::Ron,
            migrate: migrate_ui,
        },
        // The project's UI settings (ADR-2866). v2 adds the optional scale rule (ADR-8252);
        // v1 → v2 rewrites only the version number.
        Format {
            name: "uisettings",
            suffixes: &[".uisettings.ron"],
            current: regia_ui::UI_SETTINGS_VERSION,
            source: VersionSource::Ron,
            migrate: migrate_uisettings,
        },
        // The button-prompt glyph table (ADR-7070, v1 so far).
        unmigrated(
            "glyphs",
            &[".glyphs.ron"],
            regia_ui::INPUT_GLYPHS_VERSION,
            VersionSource::Ron,
        ),
        Format {
            name: "theme",
            suffixes: &[".theme.ron"],
            current: regia_ui::THEME_FORMAT_VERSION,
            source: VersionSource::Probe(read_theme_version),
            migrate: migrate_theme,
        },
        // The localization glossary (`regia_ui::loc_glossary`, v1; ADR-3359).
        unmigrated(
            "glossary",
            &[".locglossary.ron"],
            regia_ui::loc_glossary::GLOSSARY_FORMAT_VERSION,
            VersionSource::Ron,
        ),
        Format {
            name: "vfx",
            suffixes: &[".vfx.ron"],
            current: regia_vfx::VFX_FORMAT_VERSION,
            source: VersionSource::Ron,
            migrate: migrate_vfx,
        },
        // Glass crack manifests (`regia_asset::glasscrack`, v1; ADR-099).
        Format {
            name: "glasscrack",
            suffixes: &[".glasscrack.ron"],
            current: regia_asset::glasscrack::GLASSCRACK_FORMAT_VERSION,
            source: VersionSource::Ron,
            migrate: no_migration,
        },
        // Loop generator definitions (`regia_audio::loopgen`, v1; ADR-299).
        unmigrated(
            "loopgen",
            &[".loopgen.ron"],
            regia::audio::LOOPGEN_VERSION,
            VersionSource::Ron,
        ),
        // Video sources and playlists (`regia_media::source`, v1; ADR-1670).
        unmigrated(
            "video",
            &[".video.ron"],
            regia_media::source::VIDEO_SOURCE_VERSION,
            VersionSource::Ron,
        ),
        unmigrated(
            "videolist",
            &[".videolist.ron"],
            regia_media::source::VIDEO_LIST_VERSION,
            VersionSource::Ron,
        ),
        // Material graphs (v3: the `Wind` node; v4: the `Noise` node; v5: the `Code` node; v2:
        // switches, functions, vertex offset), their instances (v2: switches) and material
        // functions (v2: the `Wind` node; v3: the `Noise` node; v4: the `Code` node).
        Format {
            name: "matgraph",
            suffixes: &[".matgraph.ron"],
            current: regia_matgraph::MATGRAPH_FORMAT_VERSION,
            source: VersionSource::Ron,
            migrate: migrate_matgraph,
        },
        Format {
            name: "matinst",
            suffixes: &[".matinst.ron"],
            current: regia_matgraph::MATINST_FORMAT_VERSION,
            source: VersionSource::Ron,
            migrate: migrate_matinst,
        },
        Format {
            name: "matfunc",
            suffixes: &[".matfunc.ron"],
            current: regia_matgraph::MATFUNC_FORMAT_VERSION,
            source: VersionSource::Ron,
            migrate: migrate_matfunc,
        },
        Format {
            name: "animgraph",
            suffixes: &[".animgraph.ron"],
            current: regia_animation::ANIMGRAPH_FORMAT_VERSION,
            source: VersionSource::Ron,
            migrate: migrate_animgraph,
        },
        // The editor's animation authoring sidecar (regia_animation::authoring, v1).
        unmigrated(
            "animedit",
            &[".animedit.ron"],
            regia_animation::ANIMEDIT_FORMAT_VERSION,
            VersionSource::Ron,
        ),
        // Spring-bone chains and colliders (regia_animation::spring, v1).
        unmigrated(
            "spring",
            &[".spring.ron"],
            regia_animation::SPRING_FORMAT_VERSION,
            VersionSource::Ron,
        ),
        // Character rig drivers and skeleton profiles (regia_animation, v1).
        unmigrated(
            "drivers",
            &[".drivers.ron"],
            regia_animation::DRIVERS_FORMAT_VERSION,
            VersionSource::Ron,
        ),
        unmigrated(
            "skelprofile",
            &[".skelprofile.ron"],
            regia_animation::SKELPROFILE_FORMAT_VERSION,
            VersionSource::Ron,
        ),
        // Cues: v1 has no version field; v2 adds filter fields (ADR-272).
        Format {
            name: "cue",
            suffixes: &[".cue.ron"],
            current: regia::audio::CUE_VERSION,
            source: VersionSource::ImplicitV1,
            migrate: migrate_cue,
        },
        // Music: v1 has no version field; v2 adds stingers and transition rules (ADR-2517).
        Format {
            name: "music",
            suffixes: &[".music.ron"],
            current: regia::audio::MUSIC_VERSION,
            source: VersionSource::ImplicitV1,
            migrate: migrate_music,
        },
        // Audio patches: v2 adds one-shot and granular layers and reflected-field bindings, all
        // optional (ADR-7226).
        Format {
            name: "audiopatch",
            suffixes: &[".audiopatch.ron"],
            current: regia::audio::PATCH_VERSION,
            source: VersionSource::Ron,
            migrate: migrate_patch,
        },
        // Ambience files: v1 is the first version, nothing to migrate yet (ADR-2721).
        unmigrated(
            "ambience",
            &[".ambience.ron"],
            regia::audio::AMBIENCE_VERSION,
            VersionSource::Ron,
        ),
        // Any name ending in `terrain.layers.ron`, any case: what the cook (`discover_terrains`)
        // and the live loader (`SourceKind::TerrainLayers`) accept, so a map's plain
        // `terrain.layers.ron` migrates too (ADR-7978).
        Format {
            name: "terrain-layers",
            suffixes: &["~terrain.layers.ron"],
            current: regia_terrain::layers::TERRAIN_LAYERS_FORMAT_VERSION,
            source: VersionSource::Ron,
            migrate: migrate_terrain_layers,
        },
        // Terrain holes (tunnel mouths, regia_terrain::holes, v2: named holes, ADR-1595).
        Format {
            name: "terrain-holes",
            suffixes: &[".holes.ron"],
            current: regia_terrain::HOLES_FORMAT_VERSION,
            source: VersionSource::Probe(holes_file_version),
            migrate: migrate_holes,
        },
        Format {
            name: "scatter",
            suffixes: &[".scatter.ron"],
            current: regia_terrain::SCATTER_FORMAT_VERSION,
            source: VersionSource::Ron,
            migrate: migrate_scatter,
        },
        // Painted mesh instances (regia_terrain::scatter::painted, v1, ADR-1936).
        unmigrated(
            "scatter-paint",
            &[".scatterpaint.bin"],
            regia_terrain::SCATTER_PAINT_FORMAT_VERSION,
            VersionSource::Binary(regia_terrain::SCATTER_PAINT_MAGIC),
        ),
        // Grass fields (regia_terrain::grass, v1, ADR-1400).
        unmigrated(
            "grass",
            &[".grass.ron"],
            regia_terrain::GRASS_FORMAT_VERSION,
            VersionSource::Ron,
        ),
        // 2D toolkit (specs/22 § T2.10): tilemaps (what the Tiled importer writes) and atlases.
        // v2 added orientations (ADR-2597): a v1 map loads as is, so only the number moves.
        Format {
            name: "tilemap",
            suffixes: &[".tilemap.ron"],
            current: regia::two_d::TILEMAP_FORMAT_VERSION,
            source: VersionSource::Ron,
            migrate: migrate_tilemap,
        },
        unmigrated(
            "atlas",
            &[".atlas.ron"],
            regia::two_d::ATLAS_FORMAT_VERSION,
            VersionSource::Ron,
        ),
        // Auto-tiling rules (specs/86, ADR-1889): v2 added the WangCorner layout (ADR-2597).
        Format {
            name: "autotile",
            suffixes: &[".autotile.ron"],
            current: regia::two_d::AUTOTILE_FORMAT_VERSION,
            source: VersionSource::Ron,
            migrate: migrate_autotile,
        },
        // Saved tile patterns (specs/86 § 6, ADR-7853): v1, no migrations yet.
        unmigrated(
            "tilepatterns",
            &[".tilepatterns.ron"],
            regia::two_d::tilepatterns::TILEPATTERNS_FORMAT_VERSION,
            VersionSource::Ron,
        ),
        // Camera paths and shot sequences (`regia_framework::shots`, v3, ADR-8640).
        Format {
            name: "shots",
            suffixes: &[".shots.ron"],
            current: regia::framework::shots::SHOT_SEQUENCE_FORMAT_VERSION,
            source: VersionSource::Ron,
            migrate: migrate_shots,
        },
        // The sequencer timeline (`regia_framework::sequencer`, v5, ADR-1507, ADR-1880, ADR-2405, ADR-2862, ADR-2948).
        Format {
            name: "sequence",
            suffixes: &[".seq.ron"],
            current: regia::framework::sequencer::SEQUENCE_FORMAT_VERSION,
            source: VersionSource::Ron,
            migrate: migrate_sequence,
        },
        // Save games (`regia_framework::save_game`, v2 adds the entity id table, ADR-3239).
        Format {
            name: "save",
            suffixes: &[".save"],
            current: regia::framework::save_game::SAVE_FORMAT_VERSION,
            source: VersionSource::Binary(b"FRTS"),
            migrate: migrate_save,
        },
        unmigrated(
            "pak",
            &[".fpak"],
            regia_asset::FPAK_VERSION,
            VersionSource::Binary(b"FPAK"),
        ),
        // Cooked meshes in the dev cache (`.regia/cooked/<os>/*.fckd`, cooked mesh v2: skin,
        // colour and morph blocks). Other cooked types are skipped.
        Format {
            name: "cooked-mesh",
            suffixes: &[".fckd"],
            current: regia_asset::COOKED_MESH_FORMAT_VERSION,
            source: VersionSource::Probe(regia_asset::cooked_mesh_file_version),
            migrate: migrate_cooked_mesh,
        },
        // GI bake descriptions (`regia_pt::bake_desc`, v2 since ADR-1095 and ADR-1096, v3 lists
        // suns since ADR-7818; v0 has no version field, ADR-279).
        Format {
            name: "bake",
            suffixes: &[".bake.ron"],
            current: regia_pt::bake_desc::BAKE_DESC_FORMAT_VERSION,
            source: VersionSource::Probe(read_bake_version),
            migrate: migrate_bake,
        },
        // Cooked animation clips (same `.fckd` suffix; the probe skips other cooked types).
        // Version 2 adds the per-node morph track binding (ADR-297). Version 3 (ADR-7227) wraps
        // the body: lossless, or joint tracks compressed at cook; v1 and v2 migrate losslessly.
        Format {
            name: "cooked-clip",
            suffixes: &[".fckd"],
            current: regia_animation::clip::CLIP_FORMAT_VERSION,
            source: VersionSource::Probe(regia_animation::cooked::cooked_clip_file_version),
            migrate: migrate_cooked_clip,
        },
        // Cooked terrain indexes (same `.fckd` suffix; the probe skips other cooked types).
        // Version 2 appends the optional exact heights, holes and photoreal material (ADR-8024) and
        // the heightfield's count mapping (ADR-8471), one v2 (ADR-8619); a v1 index migrates with
        // all four empty (decode -> re-encode, cooked header kept), so it loads as before.
        // Version 3 (ADR-8871) appends the source grid's Z spacing and valid-data mask; a v1 or v2
        // index migrates with both empty the same way.
        Format {
            name: "terrain-index",
            suffixes: &[".fckd"],
            current: regia_terrain::stream::TERRAIN_INDEX_FORMAT_VERSION,
            source: VersionSource::Probe(regia_terrain::stream::terrain_index_file_version),
            migrate: migrate_terrain_index,
        },
        // Baked lightmaps (`regia_asset::lightmap`, ADR-860): `FLMP` + u32 version. Version 2
        // (ADR-1556) adds the direction section; v1 migrates by decode → re-encode with the
        // section left empty. Version 5 (ADR-2314) adds the block encodings and the denoise
        // record; v1-v4 migrate the same way, texels unchanged. Version 7 (ADR-3300) adds atlas
        // pages; v1-v6 read as one page and migrate the same way.
        Format {
            name: "lightmap",
            suffixes: &[".flmap"],
            current: regia_asset::lightmap::LIGHTMAP_FORMAT_VERSION,
            source: VersionSource::Binary(&regia_asset::lightmap::LIGHTMAP_MAGIC),
            migrate: migrate_lightmap,
        },
        // Montages and sync groups (regia_animation::montage / sync, v1, ADR-1465).
        Format {
            name: "montage",
            suffixes: &[".montage.ron"],
            current: regia_animation::MONTAGE_FORMAT_VERSION,
            source: VersionSource::Ron,
            migrate: migrate_montage,
        },
        unmigrated(
            "sync",
            &[".sync.ron"],
            regia_animation::SYNC_FORMAT_VERSION,
            VersionSource::Ron,
        ),
        // State trees (`regia_ai::state_tree`, v2 since spec 38 / ADR-1639): v2 only adds optional fields.
        Format {
            name: "statetree",
            suffixes: &[".statetree.ron"],
            current: regia_ai::state_tree::STATE_TREE_FORMAT_VERSION,
            source: VersionSource::Ron,
            migrate: migrate_statetree,
        },
        // Logic graphs (`regia_ai::logic`, spec 102 / ADR-7201): v2 adds the optional typed
        // `entity_vars` and `world_vars` (ADR-8900), so v1 → v2 rewrites the number.
        Format {
            name: "logic",
            suffixes: &[".logic.ron"],
            current: regia_ai::logic::LOGIC_FORMAT_VERSION,
            source: VersionSource::Ron,
            migrate: migrate_logic,
        },
        // Smart object definitions (`regia_ai::smart_object_def`, v1, spec 39 / ADR-1638).
        unmigrated(
            "smartobject",
            &[".smartobject.ron"],
            regia_ai::smart_object_def::SMART_OBJECT_DEF_VERSION,
            VersionSource::Ron,
        ),
        // Utility AI definitions (`regia_ai::utility`, v1, ADR-2565).
        unmigrated(
            "utility",
            &[".utility.ron"],
            regia_ai::utility::UTILITY_AI_FORMAT_VERSION,
            VersionSource::Ron,
        ),
        // Spatial query assets (`regia_ai::spatial_query`, spec 33 / ADR-1674): v2 only adds the custom
        // generator and test (ADR-2709).
        Format {
            name: "spatialquery",
            suffixes: &[".spatialquery.ron"],
            current: regia_ai::spatial_query::SPATIAL_QUERY_FORMAT_VERSION,
            source: VersionSource::Ron,
            migrate: migrate_spatialquery,
        },
        // Variant montages (regia_animation::montage_variant, v1, spec 34 M4, ADR-1644).
        unmigrated(
            "montage-variant",
            &[".montagevar.ron"],
            regia_animation::MONTAGE_VARIANT_FORMAT_VERSION,
            VersionSource::Ron,
        ),
        // The skeleton slot table (regia_animation::skeleton_slots, v1, spec 34 M1, ADR-1644).
        unmigrated(
            "slots",
            &[".slots.ron"],
            regia_animation::SLOTS_FORMAT_VERSION,
            VersionSource::Ron,
        ),
        // Procedural graphs, variants and bakes (regia_procgen, v1, ADR-1640).
        unmigrated(
            "pgraph",
            &[".pgraph.ron"],
            regia_procgen::graph::PGRAPH_FORMAT_VERSION,
            VersionSource::Ron,
        ),
        unmigrated(
            "pgvar",
            &[".pgvar.ron"],
            regia_procgen::graph::PGVAR_FORMAT_VERSION,
            VersionSource::Ron,
        ),
        // Baked procgen outputs: v2 (spec 66, ADR-1875) adds the tier and the cell hash to the
        // cell table; v1 (one untiled cell) migrates by decode -> re-encode.
        Format {
            name: "pgout",
            suffixes: &[".pgout"],
            current: regia_procgen::pgout::PGOUT_VERSION,
            source: VersionSource::Binary(&regia_procgen::pgout::PGOUT_MAGIC),
            migrate: migrate_pgout,
        },
        // Biome definitions (regia_procgen::biome, v1, ADR-1665).
        unmigrated(
            "biome",
            &[".biome.ron"],
            regia_procgen::biome::BIOME_FORMAT_VERSION,
            VersionSource::Ron,
        ),
        // The render-job file of sequence-driven render-out (regia_agent::render_job, v3, ADR-1616,
        // ADR-1880, ADR-3369).
        Format {
            name: "render-job",
            suffixes: &[".render_job.ron"],
            current: regia_agent::render_job::RENDER_JOB_FORMAT_VERSION,
            source: VersionSource::Ron,
            migrate: migrate_render_job,
        },
        // The look-review shot list of `seeing.capture_plate` (regia_agent::shot_list, v1, ADR-2463).
        unmigrated(
            "shot",
            &[".shot.ron"],
            regia_agent::shot_list::SHOT_LIST_FORMAT_VERSION,
            VersionSource::Ron,
        ),
        // Cloth and soft-body sources and cooked payloads (regia_physics::cloth::asset, v1,
        // ADR-2083). Cooked files are rebuilt from source, never migrated.
        unmigrated(
            "cloth",
            &[".cloth.ron"],
            regia::physics::cloth::asset::CLOTH_FORMAT_VERSION,
            VersionSource::Ron,
        ),
        unmigrated(
            "soft",
            &[".soft.ron"],
            regia::physics::cloth::asset::SOFT_FORMAT_VERSION,
            VersionSource::Ron,
        ),
        unmigrated(
            "cooked-cloth",
            &[".cloth.bin"],
            regia::physics::cloth::asset::COOKED_CLOTH_FORMAT_VERSION,
            VersionSource::Probe(regia::physics::cloth::asset::cooked_cloth_file_version),
        ),
        unmigrated(
            "cooked-soft",
            &[".soft.bin"],
            regia::physics::cloth::asset::COOKED_SOFT_FORMAT_VERSION,
            VersionSource::Probe(regia::physics::cloth::asset::cooked_soft_file_version),
        ),
        // Destruction level 2 chunk assets (`regia_physics::destruction`, ADR-1597; v2 adds the
        // optional structural load and falling sections, ADR-2734).
        Format {
            name: "destructible",
            suffixes: &[".destructible.ron"],
            current: regia::physics::destruction::DESTRUCTIBLE_FORMAT_VERSION,
            source: VersionSource::Ron,
            migrate: migrate_destructible,
        },
        // Destruction effects tables (`regia_framework::destruction_fx`, v1, ADR-2734).
        unmigrated(
            "destructionfx",
            &[".destructionfx.ron"],
            regia::framework::destruction_fx::DESTRUCTION_FX_FORMAT_VERSION,
            VersionSource::Ron,
        ),
        // Pre-fractured chunk sets (`regia_asset_pipeline::fracture`, v2 adds the render meshes,
        // ADR-1967).
        Format {
            name: "fracture",
            suffixes: &[".fracture.ron"],
            current: regia_asset_pipeline::fracture::FRACTURE_FORMAT_VERSION,
            source: VersionSource::Ron,
            migrate: migrate_fracture,
        },
        // Vehicle configurations (`regia_physics::vehicle_asset`, v2 adds the drivetrain, anti-roll,
        // aero and aids sections, ADR-2598; v3 the wheels' `tire_model`, ADR-2719; v4 the slip tire's
        // transients, ADR-3276).
        Format {
            name: "vehicle",
            suffixes: &[".vehicle.ron"],
            current: regia::physics::vehicle_asset::VEHICLE_FORMAT_VERSION,
            source: VersionSource::Ron,
            migrate: migrate_vehicle,
        },
        // A saved vehicle (v2 adds tire transient state, ADR-4590; v1 defaults it to zero).
        Format {
            name: "vehicle-snapshot",
            suffixes: &[".vehicle-snapshot.ron"],
            current: regia::physics::vehicle_snapshot::VEHICLE_SNAPSHOT_FORMAT_VERSION,
            source: VersionSource::Ron,
            migrate: migrate_vehicle_snapshot,
        },
        // Physics materials (`regia_physics::material`, v1, ADR-2424).
        unmigrated(
            "physmat",
            &[".physmat.ron"],
            regia::physics::material::PHYSMAT_FORMAT_VERSION,
            VersionSource::Ron,
        ),
        // Cooked convex hull sets in the dev cache (`FCKD`-wrapped `FCHS`, v1, ADR-2301); other
        // cooked types sharing the suffix are skipped by the probe.
        unmigrated(
            "hull-set",
            &[".fckd"],
            regia::physics::hull_set::HULL_SET_FORMAT_VERSION,
            VersionSource::Probe(regia::physics::hull_set::cooked_hull_set_file_version),
        ),
        // Data-driven gameplay cameras (`regia_framework::camera::asset`, v2 adds the `Rail` and
        // `Crane` node kinds, spec 35, ADR-1637, ADR-3231).
        Format {
            name: "camera",
            suffixes: &[".camera.ron"],
            current: regia::framework::camera::asset::CAMERA_FORMAT_VERSION,
            source: VersionSource::Ron,
            migrate: migrate_camera,
        },
        // Voice chat project settings (`regia_audio::voice_chat`, v1, spec 59, ADR-1647).
        unmigrated(
            "voice-chat",
            &["voice_chat.ron"],
            regia::audio::voice_chat::CONFIG_FORMAT_VERSION,
            VersionSource::Ron,
        ),
        // The project's weather config (`regia_vfx::weather`, v1, spec 99, ADR-1847).
        unmigrated(
            "weather",
            &["weather.ron"],
            regia_vfx::weather::WEATHER_CONFIG_FORMAT_VERSION,
            VersionSource::Ron,
        ),
        // Curve tables and choosers (`regia_framework::{curve_table, chooser}`, v1, spec 51) and
        // attributes, effects and abilities (`regia_framework::abilities`, v1, spec 53); ADR-1695.
        unmigrated(
            "curves",
            &[".curves.ron"],
            regia::framework::curve_table::CURVE_TABLE_FORMAT_VERSION,
            VersionSource::Ron,
        ),
        unmigrated(
            "chooser",
            &[".chooser.ron"],
            regia::framework::chooser::CHOOSER_FORMAT_VERSION,
            VersionSource::Ron,
        ),
        unmigrated(
            "attrs",
            &[".attrs.ron"],
            regia::framework::abilities::ATTRIBUTES_FORMAT_VERSION,
            VersionSource::Ron,
        ),
        unmigrated(
            "effect",
            &[".effect.ron"],
            regia::framework::abilities::EFFECT_FORMAT_VERSION,
            VersionSource::Ron,
        ),
        unmigrated(
            "ability",
            &[".ability.ron"],
            regia::framework::abilities::ABILITY_FORMAT_VERSION,
            VersionSource::Ron,
        ),
        // Ragdoll profiles (`regia_framework::ragdoll::file`, v2 adds body shapes, spec 93, ADR-1693,
        // ADR-2133 § 6; v3 adds the active-ragdoll block, ADR-2498; v4 the Sole shape and the recovery block, ADR-2963).
        Format {
            name: "ragdoll",
            suffixes: &[".ragdoll.ron"],
            current: regia::framework::ragdoll::file::RAGDOLL_FORMAT_VERSION,
            source: VersionSource::Ron,
            migrate: migrate_ragdoll,
        },
        // Motion-matching database sources (`regia_animation::motion_matching::asset`, v2 adds the
        // optional contact labels and the contact and phase weights, spec 52 § 4, ADR-1831,
        // ADR-2858).
        Format {
            name: "motiondb",
            suffixes: &[".motiondb.ron"],
            current: regia_animation::motion_matching::MOTIONDB_FORMAT_VERSION,
            source: VersionSource::Ron,
            migrate: migrate_motiondb,
        },
        // Cooked motion databases (same `.fckd` suffix; the probe skips other cooked types). v3
        // adds the optional arc-length and clip-speed arrays (spec 52 § 4, ADR-7817): v1 and v2
        // are rewritten with the clip speeds; the arc lengths come with the next cook.
        Format {
            name: "motiondb-cooked",
            suffixes: &[".fckd"],
            current: regia_animation::motion_matching::COOKED_MOTIONDB_VERSION,
            source: VersionSource::Probe(regia_animation::cooked::cooked_motion_database_file_version),
            migrate: migrate_cooked_motion_database,
        },
        // Retarget setups (regia_animation::retarget_setup, ADR-1857; v2 adds the IK goals solved at
        // bake, ADR-8476).
        Format {
            name: "retarget",
            suffixes: &[".retarget.ron"],
            current: regia_animation::RETARGET_FORMAT_VERSION,
            source: VersionSource::Ron,
            migrate: migrate_retarget,
        },
        // Authored off-mesh links (`regia_nav::link_file`, v1, spec 92, ADR-1821).
        unmigrated(
            "navlinks",
            &[".navlinks.ron"],
            regia_nav::NAV_LINKS_FORMAT_VERSION,
            VersionSource::Ron,
        ),
        // Authored crowd lanes (`regia_nav::lanes`, v2 adds lane weights and opposite pairs, ADR-1934,
        // ADR-2133).
        Format {
            name: "navlanes",
            suffixes: &[".navlanes.ron"],
            current: regia_nav::NAV_LANES_FORMAT_VERSION,
            source: VersionSource::Ron,
            migrate: migrate_navlanes,
        },
        // Demo recordings (`regia_framework::demo`, v2 adds input timestamps, ADR-1878) and their
        // binary match-record form (`.frec`, v1, RR-7).
        Format {
            name: "demo",
            suffixes: &[".regiademo"],
            current: regia::framework::demo::DEMO_FORMAT_VERSION,
            source: VersionSource::Probe(demo_file_version),
            migrate: migrate_demo,
        },
        unmigrated(
            "frec",
            &[".frec"],
            regia::framework::input_recording::FREC_FORMAT_VERSION,
            VersionSource::Binary(regia::framework::input_recording::FREC_MAGIC),
        ),
        // A network recording (spec 103): a file belongs to its protocol, so nothing rewrites it.
        unmigrated(
            "netdemo",
            &[".netdemo"],
            regia_net::recorder::format::NETDEMO_FORMAT_VERSION,
            VersionSource::Probe(regia_net::recorder::format::netdemo_file_version),
        ),
        // A client capture (spec 120 § 15.5, ADR-8221): it belongs to its protocol, so nothing
        // rewrites it.
        unmigrated(
            "netcapture",
            &[".netcapture"],
            regia_net::capture::CAPTURE_FORMAT_VERSION,
            VersionSource::Probe(regia_net::capture::netcapture_file_version),
        ),
        // Authored collision proxies of a glTF (the `collision` sub-asset, same `.fckd` suffix;
        // the probe skips other cooked types; v1, ADR-2341).
        unmigrated(
            "collision-set",
            &[".fckd"],
            regia_asset::COLLISION_SET_FORMAT_VERSION,
            VersionSource::Probe(regia_asset::collision_set_file_version),
        ),
        // The instanced nodes of a glTF (the `prefab/instances` sub-asset, same `.fckd` suffix; the
        // probe skips other cooked types; v1, ADR-3016).
        unmigrated(
            "prefab-instances",
            &[".fckd"],
            regia_asset::PREFAB_INSTANCES_FORMAT_VERSION,
            VersionSource::Probe(regia_asset::prefab_instances_file_version),
        ),
        // The format-registry sweep (ADR-2316). The lobby keys and the cooked nav cell are not files
        // (lobby platform data, bytes inside a cell blob), so they have no entry here.
        Format {
            name: "net-settings",
            suffixes: &["/net.ron"],
            current: regia_net::settings::NET_SETTINGS_FORMAT_VERSION,
            source: VersionSource::Ron,
            migrate: migrate_net_settings,
        },
        // v1 → v2 adds the optional `rate_tiers` (ADR-8031) and `extrapolation_gate_m` (off when
        // absent, ADR-8522): the step rewrites the version number.
        Format {
            name: "netprofile",
            suffixes: &[".netprofile.ron"],
            current: regia_net::profile::NET_PROFILE_FORMAT_VERSION,
            source: VersionSource::Ron,
            migrate: migrate_net_profile,
        },
        // `server.toml` v2 adds the optional `[persistence]` section (ADR-3354): the step rewrites
        // the version line and keeps every other byte.
        Format {
            name: "server-toml",
            suffixes: &["/server.toml"],
            current: regia_net::server_host::config::SERVER_TOML_FORMAT_VERSION,
            source: VersionSource::Probe(server_toml_version),
            migrate: migrate_server_toml,
        },
        Format {
            name: "playlist-ron",
            suffixes: &[".playlist.ron"],
            current: regia_net::formats::PLAYLIST_RON_FORMAT_VERSION,
            source: VersionSource::Ron,
            migrate: migrate_playlist_ron,
        },
        // v1 → v2: a bare IPv6 entry becomes `addr/128`, its v1 exact-address meaning (ADR-4936).
        Format {
            name: "bans",
            suffixes: &["/bans.ron"],
            current: regia_net::session::bans::BAN_LIST_FORMAT_VERSION,
            source: VersionSource::Ron,
            migrate: migrate_bans,
        },
        unmigrated(
            "net-manifest",
            &["/net-manifest.ron"],
            regia_net::session::manifest::NET_MANIFEST_FORMAT_VERSION,
            VersionSource::Ron,
        ),
        unmigrated(
            "server-key",
            &["/server.key"],
            regia_net::crypto::SERVER_KEY_FORMAT_VERSION,
            VersionSource::Ron,
        ),
        unmigrated(
            "siminput",
            &[".siminput.ron"],
            regia_net::session::hub::SIM_RECORDING_FORMAT_VERSION,
            VersionSource::Ron,
        ),
        // The lockstep desync bundle (spec 120 § 7.6, ADR-8455): a diagnostic file, version 1.
        unmigrated(
            "desync",
            &[".desync.ron"],
            regia_net::desync::DESYNC_BUNDLE_FORMAT_VERSION,
            VersionSource::Ron,
        ),
        // The lockstep match replay (ADR-8513): a recording belongs to its game build, so nothing
        // rewrites it; v1.
        unmigrated(
            "lsreplay",
            &[".lsreplay"],
            regia_net::lockstep::replay::LSREPLAY_VERSION,
            VersionSource::Probe(regia_net::lockstep::replay::lsreplay_file_version),
        ),
        unmigrated(
            "xr-config",
            &["/xr.ron"],
            regia_xr::XR_CONFIG_FORMAT_VERSION,
            VersionSource::Ron,
        ),
        unmigrated(
            "xr-actions",
            &[".xr.ron"],
            regia_xr::input::MANIFEST_FORMAT_VERSION,
            VersionSource::Ron,
        ),
        unmigrated(
            "xrsim",
            &[".xrsim.ron"],
            regia_xr::sim::SIM_FORMAT_VERSION,
            VersionSource::Ron,
        ),
        unmigrated(
            "engine-release",
            &["/regia-engine.release.ron"],
            regia_cli::distribution::RELEASE_FORMAT_VERSION,
            VersionSource::Ron,
        ),
        Format {
            name: "session",
            suffixes: &[".session.ron"],
            current: regia_cli::session::SESSION_FILE_VERSION,
            source: VersionSource::Ron,
            migrate: migrate_session,
        },
        unmigrated(
            "validation",
            &["/validation.ron"],
            regia_asset_pipeline::validation_rules::VALIDATION_FILE_FORMAT_VERSION,
            VersionSource::Ron,
        ),
        unmigrated(
            "launch-profiles",
            &["/launch_profiles.ron"],
            regia_asset_pipeline::launch_profiles::FORMAT_VERSION,
            VersionSource::Ron,
        ),
        unmigrated(
            "import-rules",
            &["/import_rules.ron"],
            regia_asset_pipeline::import_rules::IMPORT_RULES_FORMAT_VERSION,
            VersionSource::Ron,
        ),
        unmigrated(
            "assetpkg",
            &["/.regia_assetpkg.ron"],
            regia_asset_pipeline::asset_package::ASSET_PACKAGE_FORMAT_VERSION,
            VersionSource::Ron,
        ),
        // UI animation libraries: v2 adds the optional frame `events` (ADR-8900); v1 → v2 rewrites
        // (or, for a file without the field, writes) the number.
        Format {
            name: "uianim",
            suffixes: &[".uianim.ron"],
            current: regia_ui::anim_timeline::UIANIM_FORMAT_VERSION,
            source: VersionSource::ImplicitV1,
            migrate: migrate_uianim,
        },
        // The asset id manifest (ADR-8460; v1 so far).
        unmigrated(
            "asset-ids",
            &[".assetids.ron"],
            regia_asset::asset_ids::ASSET_IDS_FORMAT_VERSION,
            VersionSource::Ron,
        ),
        unmigrated(
            "data-asset",
            &[".data.ron"],
            regia_asset::data_asset::DATA_ASSET_FORMAT_VERSION,
            VersionSource::Ron,
        ),
        Format {
            name: "data-table",
            suffixes: &[".table.ron"],
            current: regia_asset::data_table::DATA_TABLE_FORMAT_VERSION,
            source: VersionSource::Probe(data_table_version),
            migrate: migrate_data_table,
        },
        unmigrated(
            "asset-tags",
            &["/asset_tags.ron"],
            regia_asset::registry::TAG_RULES_FORMAT_VERSION,
            VersionSource::Ron,
        ),
        // Content Browser collections and favorites (ADR-7759; v1 so far).
        unmigrated(
            "asset-collections",
            &["/asset_collections.ron"],
            regia_asset::asset_collections::ASSET_COLLECTIONS_FORMAT_VERSION,
            VersionSource::Ron,
        ),
        // Named import presets per importer (ADR-7904; v1 so far).
        unmigrated(
            "import-presets",
            &["/import_presets.ron"],
            regia_asset::import_presets::IMPORT_PRESETS_FORMAT_VERSION,
            VersionSource::Ron,
        ),
        unmigrated(
            "asset-registry",
            &["/asset_registry.bin", ".registry.bin"],
            regia_asset::registry::REGISTRY_FORMAT_VERSION,
            VersionSource::Probe(asset_registry_version),
        ),
        Format {
            name: "input-overrides",
            suffixes: &["/input_overrides.ron"],
            current: regia_input::OVERRIDES_VERSION,
            source: VersionSource::ImplicitV1,
            migrate: migrate_input_overrides,
        },
        unmigrated(
            "permutations",
            &["/permutations.json"],
            regia_render::permutation_policy::PERMUTATION_MANIFEST_FORMAT_VERSION,
            VersionSource::Probe(permutations_version),
        ),
        unmigrated(
            "convc",
            &[".convc"],
            regia::framework::conversation::cooked::CONVC_VERSION,
            VersionSource::Probe(postcard_version),
        ),
        Format {
            name: "bt",
            suffixes: &[".bt.ron"],
            current: regia_ai::tree::BT_FORMAT_VERSION,
            source: VersionSource::ImplicitV1,
            migrate: migrate_bt,
        },
        unmigrated(
            "terrain-weights",
            &[".weights.bin"],
            regia_terrain::LAYER_WEIGHTS_FORMAT_VERSION,
            VersionSource::Probe(postcard_version),
        ),
        // Prefab variants (regia_scene::prefab_variant, v1, ADR-2323): a file without
        // `format_version` reads as v0 and is re-saved at v1.
        Format {
            name: "prefab-variant",
            suffixes: &[regia_scene::prefab_variant::PREFAB_VARIANT_SUFFIX],
            current: regia_scene::prefab_variant::PREFAB_VARIANT_FORMAT_VERSION,
            source: VersionSource::Probe(read_prefab_variant_version),
            migrate: migrate_prefab_variant,
        },
        // Profile captures (regia_telemetry::profile_capture, v2 since ADR-7325): a file without
        // `format_version` reads as v0; v0 and v1 files are re-written at v2 with no timeline.
        Format {
            name: "profile-capture",
            suffixes: &[regia_telemetry::profile_capture::PROFILE_CAPTURE_SUFFIX],
            current: regia_telemetry::profile_capture::PROFILE_CAPTURE_FORMAT_VERSION,
            source: VersionSource::Probe(read_profile_capture_version),
            migrate: migrate_profile_capture,
        },
        // Cooked world indexes of streamed levels (`.fckd`, regia_world::format, v2 since ADR-2483:
        // the baked HLOD proxy atlases). The probe skips other cooked types.
        Format {
            name: "world-index",
            suffixes: &[".fckd"],
            current: regia_world::format::WORLD_INDEX_FORMAT_VERSION,
            source: VersionSource::Probe(read_world_index_version),
            migrate: migrate_world_index,
        },
        // Cooked IES photometric profiles in the dev cache (`FCKD`-wrapped `FIES`, v1, ADR-2592); other
        // cooked types sharing the suffix are skipped by the probe.
        unmigrated(
            "ies-profile",
            &[".fckd"],
            regia_asset::ies::IES_PROFILE_FORMAT_VERSION,
            VersionSource::Probe(regia_asset::ies::cooked_ies_profile_file_version),
        ),
        // Baked particle-liquid runs (regia_fluid::cache, v1, ADR-2441); binary, the probe skips other bytes.
        unmigrated(
            "fluidcache",
            &[".fluidcache"],
            regia_fluid::cache::FLUIDCACHE_FORMAT_VERSION,
            VersionSource::Probe(regia_fluid::cache::fluidcache_file_version),
        ),
        // The client's trust-on-first-use store (ADR-2416): machine-local, v1, no migrator yet.
        unmigrated(
            "known-hosts",
            &["/known_hosts.ron"],
            regia_net::known_hosts::KNOWN_HOSTS_FORMAT_VERSION,
            VersionSource::Ron,
        ),
        // Painted grass density (`<map>.grassdensity.bin`, postcard, version first; ADR-2411).
        unmigrated(
            "grass-density",
            &[".grassdensity.bin"],
            regia_terrain::GRASS_DENSITY_FORMAT_VERSION,
            VersionSource::Probe(postcard_version),
        ),
        // Non-destructive terrain edit layers (`<map>.terrain.editlayers`, postcard, version first;
        // ADR-7825, v2 since ADR-8381): v1 migrates with empty weights and no procgen target.
        Format {
            name: "terrain-editlayers",
            suffixes: &[".terrain.editlayers"],
            current: regia_terrain::EDIT_LAYERS_FORMAT_VERSION,
            source: VersionSource::Probe(postcard_version),
            migrate: migrate_edit_layers,
        },
        // The deterministic navmesh (`<map>.detnav`, `NVM2` + u32 version; DO27's cooked mesh copied
        // whole, ADR-8174): enters at v2, no older version, no migrator.
        unmigrated(
            regia_nav::det::navmesh::NAVMESH_FORMAT,
            &[regia_nav::det::navmesh::NAVMESH_SUFFIX],
            regia_nav::det::navmesh::NAVMESH_VERSION,
            VersionSource::Binary(&regia_nav::det::navmesh::NAVMESH_MAGIC),
        ),
        // Mesh layers and world geometry source (`<map>.meshlayers.ron`, spec 121 § 9.1, ADR-8112) and the
        // passage description a `Passages` layer names (`<map>.passages.ron`, same format family and
        // version): v1, no migrator yet; a newer file is refused by the loader.
        unmigrated(
            "terrain-mesh-layers",
            &[".meshlayers.ron", ".passages.ron"],
            regia_world_geom::meshlayers::MESH_LAYERS_FORMAT_VERSION,
            VersionSource::Probe(meshlayers_version),
        ),
        // The cooked `<map>.meshlayers` (magic `RGML`, postcard, spec 121 § 9.2, ADR-8112): derived data; the
        // probe skips other bytes. v1 (v0.6.0) -> v2 (format batch 2): the canonical section's counts become
        // fixed width behind a hashed head (ADR-8816; the value and `content_hash` are unchanged) and the
        // chunk section migrates with every layer's `collider` false (ADR-8872); `regia cook` writes the real
        // flag when the document sets it.
        Format {
            name: "terrain-mesh-layers-cooked",
            suffixes: &[".meshlayers"],
            current: regia_world_geom::cooked::COOKED_FORMAT_VERSION,
            source: VersionSource::Probe(regia_world_geom::cooked::cooked_file_version),
            migrate: migrate_meshlayers_cooked,
        },
        // The provenance policy (`config/provenance.ron`, v1, ADR-2428).
        unmigrated(
            "provenance-policy",
            &["/provenance.ron"],
            regia_asset_pipeline::provenance_policy::PROVENANCE_POLICY_FORMAT_VERSION,
            VersionSource::Ron,
        ),
        // Generator declarations (`generators/<name>.toml`, v1, ADR-2412): a `.toml` with a top-level
        // `[generator]` table; every other TOML file probes as not this format.
        unmigrated(
            "generator",
            &[".toml"],
            regia_asset_pipeline::generators::GENERATOR_FORMAT_VERSION,
            VersionSource::Probe(regia_asset_pipeline::generators::generator_file_version),
        ),
        // Product viewer contracts (`jewelry.<name>/<major>` JSON, ADR-2480, ADR-2535): any `.json` file
        // whose `schema` names the interface; every other JSON file probes as not this format.
        unmigrated(
            regia_configurator::Interface::PartKit.format_name(),
            &[".json"],
            regia_configurator::CONTRACT_MAJOR,
            VersionSource::Probe(jewelry_part_kit_version),
        ),
        unmigrated(
            regia_configurator::Interface::MaterialLibrary.format_name(),
            &[".json"],
            regia_configurator::CONTRACT_MAJOR,
            VersionSource::Probe(jewelry_material_library_version),
        ),
        unmigrated(
            regia_configurator::Interface::Build.format_name(),
            &[".json"],
            regia_configurator::CONTRACT_MAJOR,
            VersionSource::Probe(jewelry_build_version),
        ),
        unmigrated(
            regia_configurator::Interface::ProductPackage.format_name(),
            &[".json"],
            regia_configurator::CONTRACT_MAJOR,
            VersionSource::Probe(jewelry_product_package_version),
        ),
        // The product viewer's baked environment (ADR-2539): a baked output, baked again from its
        // source image on a layout change.
        unmigrated(
            "viewer-env",
            &[".renv"],
            regia_viewer::ENV_FORMAT_VERSION,
            VersionSource::Binary(b"RENV"),
        ),
        // Product viewer packages (`manifest.json` of `regia.viewer_package/1` and `mesh.rvm`;
        // cooked data with no migrator: a version change is cooked again with `regia viewer
        // pack`, ADR-2540). Other JSON files are skipped by the probe.
        unmigrated(
            "viewer-package",
            &[".json", ".rvm"],
            regia_viewer_pack::FORMAT_VERSION,
            VersionSource::Probe(regia_viewer_pack::probe_version),
        ),
        // Finished-match records (`<root>/<utc-date>/<match_id>.json`, audit gap GR-10.4); other JSON
        // files are skipped by the probe. v1 is the first version: the migrator is a stub that a
        // version bump fills in.
        Format {
            name: "match-result",
            suffixes: &[".json"],
            current: regia_framework::match_results::MATCH_RESULT_FORMAT_VERSION,
            source: VersionSource::Probe(regia_framework::match_results::probe_version),
            migrate: regia_framework::match_results::migrate_match_result,
        },
        // Cooked voxel far proxies in the dev cache (`FCKD`-wrapped `FVOX`, ADR-2445); other cooked
        // types sharing the suffix are skipped by the probe. v2 (ADR-2655) adds the sub-voxel
        // boxes: a v1 file is rewritten with every box full and its cube-diagonal error (it looks
        // the same); `regia cook` gives it tight boxes. v3 (ADR-3108) adds the sampled box errors
        // and v4 (ADR-3227) the per-level coverage estimate: older files are rewritten without
        // them, and a recook measures them.
        Format {
            name: "voxel-far",
            suffixes: &[".fckd"],
            current: regia::render::visbuffer::VOXEL_FAR_FORMAT_VERSION,
            source: VersionSource::Probe(regia::render::visbuffer::cooked_voxel_far_file_version),
            migrate: migrate_voxel_far,
        },
        // Channel-pack recipes (`.texpack.ron`, v1, ADR-3090).
        unmigrated(
            "texpack",
            &[".texpack.ron"],
            regia_asset_pipeline::texture_pack::TEXPACK_FORMAT_VERSION,
            VersionSource::Ron,
        ),
        // Entity kind declarations (`.kind.ron`, v1, spec 101 § 2.3, ADR-2488).
        unmigrated(
            "kind",
            &[".kind.ron"],
            regia_scene::kind::file::KIND_FORMAT_VERSION,
            VersionSource::Ron,
        ),
        // Baked GI volumes (`regia_asset::baked_gi`, `FGIV` + u32 version). Version 4 (ADR-2618)
        // adds the split section, version 5 (ADR-3237) its cropped bricks; v1-v4 migrate by decode →
        // re-encode (v1-v3 no split, v4 parts no crop, every section byte for byte).
        Format {
            name: "baked-gi",
            suffixes: &[".fgiv"],
            current: regia_asset::baked_gi::BAKED_GI_VERSION,
            source: VersionSource::Binary(&regia_asset::baked_gi::BAKED_GI_MAGIC),
            migrate: migrate_baked_gi,
        },
        // Trailer configurations (`regia_physics::vehicle_trailer_asset`, v2, ADR-3281; v1 migrates).
        Format {
            name: "trailer",
            suffixes: &[".trailer.ron"],
            current: regia::physics::vehicle_trailer_asset::TRAILER_FORMAT_VERSION,
            source: VersionSource::Ron,
            migrate: migrate_trailer,
        },
        // Fixed-wing aircraft (`regia_physics::aero::asset`, v1, ADR-3374).
        unmigrated(
            "aircraft",
            &[".aircraft.ron"],
            regia::physics::aero::asset::AIRCRAFT_FORMAT_VERSION,
            VersionSource::Ron,
        ),
        // Local reflection capture bakes (`.fcap`, `RFCP` magic then the version; v1, ADR-2860).
        unmigrated(
            "fcap",
            &[".fcap"],
            regia_asset::local_capture_bake::FCAP_FORMAT_VERSION,
            VersionSource::Binary(&regia_asset::local_capture_bake::FCAP_MAGIC),
        ),
        // The project's bus tree `config/audio.ron` (`audio-buses`, ADR-8105 (g), ADR-8354): v0 is the
        // versionless shape; v1 adds the version, snapshots and bypass. The probe claims an `audio.ron`
        // only when it parses as a bus layout, so another file of that name is left alone.
        Format {
            name: "audio-buses",
            suffixes: &["/audio.ron"],
            current: regia::audio::AUDIO_BUSES_FORMAT_VERSION,
            source: VersionSource::Probe(read_audio_buses_version),
            migrate: migrate_audio_buses,
        },
        // Vehicle sound definitions (`regia_audio::vehicle_audio`, v1, ADR-3271).
        unmigrated(
            "vehicle-audio",
            &[".vehicleaudio.ron"],
            regia::audio::VEHICLE_AUDIO_VERSION,
            VersionSource::Ron,
        ),
        // Bake checkpoints (`regia_asset::bake_checkpoint`, v1, ADR-3095): transient files beside a
        // bake's output, never migrated (a bake of another version starts over); binary, the probe
        // skips other bytes.
        unmigrated(
            "bake-checkpoint",
            &[".bakeck"],
            regia_asset::bake_checkpoint::BAKE_CHECKPOINT_FORMAT_VERSION,
            VersionSource::Probe(regia_asset::bake_checkpoint::bake_checkpoint_file_version),
        ),
        // Cooked HLOD voxel tables of streamed levels with scatter (`.fckd`, regia_world::hlod_voxels,
        // v1, ADR-3253); the probe skips other cooked types. No migrator yet (v1 is the first).
        unmigrated(
            "hlod-voxels",
            &[".fckd"],
            regia_world::hlod_voxels::HLOD_VOXELS_FORMAT_VERSION,
            VersionSource::Probe(read_hlod_voxels_version),
        ),
        // Contact-sound banks (`regia_audio::impacts`, v1, ADR-3270).
        unmigrated(
            "impactbank",
            &[".impactbank.ron"],
            regia::audio::impacts::IMPACT_BANK_FORMAT_VERSION,
            VersionSource::Ron,
        ),
        // Projected-decal prefabs (`regia_vfx::decal_format`): v2 adds `metallic` and `shading` for
        // forward decals (ADR-3255); v3 adds optional custom textures (ADR-8121).
        Format {
            name: "decal",
            suffixes: &[".decal.ron"],
            current: regia_vfx::decal_format::DECAL_FORMAT_VERSION,
            source: VersionSource::Ron,
            migrate: migrate_decal,
        },
        // The cell index of `regia bake-lightmaps --cells` (`regia_pt::lightmap::cells`, v1, ADR-3318): a
        // bake cache beside the cell lightmaps, never migrated (an index of another version re-bakes every
        // cell); the probe skips a `cells.json` that is not an index.
        unmigrated(
            "lightmap-cells",
            &["cells.json"],
            regia_pt::lightmap::cells::CELL_BAKE_INDEX_VERSION,
            VersionSource::Probe(regia_pt::lightmap::cells::cell_bake_index_file_version),
        ),
        // Indoor cells and portals (`regia_render::viscells`, v1, spec 25 § 7.7, ADR-7586): the first
        // version, so nothing migrates yet; a newer file is refused by the loader.
        unmigrated(
            "viscells",
            &[".viscells.ron"],
            regia_render::viscells::VISCELLS_FORMAT_VERSION,
            VersionSource::Probe(viscells_file_version),
        ),
        // Level variants (`regia_scene::level_variant`, v1, ADR-7823): a file without
        // `format_version` reads as v0 and is re-saved at v1.
        Format {
            name: "level-variant",
            suffixes: &[regia_scene::level_variant::LEVEL_VARIANT_SUFFIX],
            current: regia_scene::level_variant::LEVEL_VARIANT_FORMAT_VERSION,
            source: VersionSource::Probe(read_prefab_variant_version),
            migrate: migrate_level_variant,
        },
        // Formation slot lists (`.formation.ron` v1, ADR-8358). Nothing older to migrate.
        unmigrated(
            "ai-formation",
            &[".formation.ron"],
            regia_ai::FORMATION_FORMAT_VERSION,
            VersionSource::Ron,
        ),
        // Key tracks (`.track.ron`, `regia_cli::key_track`, v1, ADR-8403): the first version, so
        // nothing migrates yet. A newer file is refused by the loader.
        unmigrated(
            "key-track",
            &[".track.ron"],
            regia_cli::key_track::KEY_TRACK_FORMAT_VERSION,
            VersionSource::Ron,
        ),
        // Per-class delivery contract (`.contract.ron`, v1, ADR-8401): the first version, so nothing
        // migrates. A newer file is refused by the loader.
        unmigrated(
            "delivery-contract",
            &[regia_asset::delivery_contract::DELIVERY_CONTRACT_SUFFIX],
            regia_asset::delivery_contract::DELIVERY_CONTRACT_FORMAT_VERSION,
            VersionSource::Ron,
        ),
        // Project source-lint policy (`config/source_lint.ron`, v1, ADR-8248). A missing file
        // is the engine default; an older version does not exist, so nothing migrates.
        unmigrated(
            "source-lint",
            &["/source_lint.ron"],
            regia_asset_pipeline::source_lint::SOURCE_LINT_POLICY_VERSION,
            VersionSource::Ron,
        ),
        // Release chunks (`release-chunks.json`, v1, ADR-8404). The whole file name is owned.
        // v1 re-encodes; a newer file is refused and the error names the version.
        Format {
            name: "release-chunks",
            suffixes: &["/release-chunks.json"],
            current: regia_release_chunks::FORMAT_VERSION,
            source: VersionSource::Probe(release_chunks_version),
            migrate: migrate_release_chunks,
        },
        // Named hover hulls (`.hoverhull.ron`, ADR-8512). v1 is the only version.
        unmigrated(
            "hoverhull",
            &[".hoverhull.ron"],
            regia::framework::det_hover::HOVERHULL_FORMAT_VERSION,
            VersionSource::Ron,
        ),
        // Two-wheel bikes (`regia_physics::vehicle_bike::asset`, v1, ADR-8630). v1 is the only version.
        unmigrated(
            "bike",
            &[".bike.ron"],
            regia::physics::vehicle_bike::asset::BIKE_FORMAT_VERSION,
            VersionSource::Ron,
        ),
        // Deterministic utility catalogues (`.detutility.ron` v1, gap AI-7): the first version, so
        // nothing migrates; registered so v2 can. A newer file is refused by the loader.
        unmigrated(
            regia_ai::det::DET_UTILITY_FORMAT,
            &[regia_ai::det::DET_UTILITY_SUFFIX],
            regia_ai::det::DET_UTILITY_FORMAT_VERSION,
            VersionSource::Ron,
        ),
        // Terrain source descriptors (ADR-8639): the count-raster layout (`heightfield.raster.json`,
        // ADR-8542) and a heightfield in several files (`heightfield.tiles.json`, ADR-8595). v1, the
        // first version: listed and left in place; a newer file is refused by the reader. The overview
        // descriptor (`<name>.overview.json`) is a cache and is not listed.
        unmigrated(
            "terrain-raster-descriptor",
            &[".raster.json"],
            regia_terrain::source::RASTER_DESCRIPTOR_VERSION,
            VersionSource::Probe(terrain_raster_descriptor_version),
        ),
        unmigrated(
            "terrain-source-tiles",
            &[".tiles.json"],
            regia_terrain::source::TILE_SET_VERSION,
            VersionSource::Probe(terrain_source_tiles_version),
        ),
        // Rule sets (`.rules.ron`, a match mode as data, ADR-8694). v1 is the first version: a current
        // file is parsed and checked and stays as it is; any other version is refused.
        Format {
            name: "rules-ron",
            suffixes: &[".rules.ron"],
            current: regia::framework::rules::ruleset::RULES_RON_FORMAT_VERSION,
            source: VersionSource::Ron,
            migrate: migrate_rules_ron,
        },
        // Lobby hub config (`hub.toml`, v1, ADR-8648). The first version, so nothing migrates.
        unmigrated(
            "hub-toml",
            &["/hub.toml"],
            regia_net::lobby::hub::HUB_TOML_FORMAT_VERSION,
            VersionSource::Probe(hub_toml_version),
        ),
        // Release manifest signature sidecars (`manifest-<platform>.json.sig`, v1, ADR-8687,
        // registered by ADR-8846). v1 is the first version: listed, left in place.
        unmigrated(
            regia_release_chunks::RELEASE_SIGNATURE_FORMAT_NAME,
            &[regia_release_chunks::RELEASE_SIGNATURE_SUFFIX],
            regia_release_chunks::RELEASE_SIGNATURE_FORMAT,
            VersionSource::Probe(release_signature_version),
        ),
        // Gait settings (`.gait.ron` v1, gap G7): the first version, so nothing migrates yet and a v1
        // file passes through; a newer file is refused by the loader (`gait_version`).
        unmigrated(
            regia_animation::gait_asset::GAIT_FORMAT,
            &[regia_animation::gait_asset::GAIT_SUFFIX],
            regia_animation::gait_asset::GAIT_FORMAT_VERSION,
            VersionSource::Ron,
        ),
    ];
    // The editor's own files (ADR-7902): their types live in the desktop-only `regia_editor`, so the
    // registry reads the descriptors of `regia_core::editor_formats`. A user-config file is never
    // under a project, so only the project-local ones (`.regia/editor_layout.ron`,
    // `.regia/editor_profile.ron`, `.regia/editor_view.ron`, `.regia/viewport_snap.ron`) and
    // files a project ships on purpose are ever found.
    formats.extend(regia_core::editor_formats::EDITOR_FORMATS.iter().map(|f| Format {
        name: f.name,
        suffixes: f.files,
        current: f.version,
        source: VersionSource::Ron,
        migrate: editor_migrator(f.name),
    }));
    formats
}

/// `release-chunks.json` (ADR-8404). `Ok(None)` is unused: a file of this name that is not the
/// format is `manifest_invalid`, which the sweep reports.
fn release_chunks_version(bytes: &[u8]) -> Result<Option<u32>, String> {
    regia_release_chunks::peek_version(bytes)
        .map(Some)
        .map_err(|err| err.message)
}

/// Re-encodes a release-chunk manifest at the current version. A newer file's error names the version.
fn migrate_release_chunks(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    regia_release_chunks::migrate_json(bytes).map_err(|err| format!("{} ({})", err.message, err.code))
}

/// `.levelvariant.ron` v0 to v1 (ADR-7823): load (the loader migrates), then re-save.
fn migrate_level_variant(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    let file = regia_scene::level_variant::LevelVariantFile::from_ron_str(text).map_err(|e| e.to_string())?;
    let out = file.to_ron_string().map_err(|e| e.to_string())?;
    Ok(out.into_bytes())
}

/// `regia upgrade-project` probes for the product viewer contracts (ADR-2480, ADR-2535): the major of
/// a `.json` file whose `schema` is `jewelry.<name>/<major>`, `None` for any other file.
macro_rules! jewelry_probes {
    ($($fn_name:ident => $interface:ident),* $(,)?) => {
        $(
            fn $fn_name(bytes: &[u8]) -> Result<Option<u32>, String> {
                regia_configurator::Interface::$interface.probe_file_version(bytes)
            }
        )*
    };
}

jewelry_probes! {
    jewelry_part_kit_version => PartKit,
    jewelry_material_library_version => MaterialLibrary,
    jewelry_build_version => Build,
    jewelry_product_package_version => ProductPackage,
}

/// The version of a cooked HLOD voxel table (`Ok(None)` for another cooked type, ADR-3253).
fn read_hlod_voxels_version(bytes: &[u8]) -> Result<Option<u32>, String> {
    regia_world::hlod_voxels::hlod_voxels_file_version(bytes).map_err(|e| e.to_string())
}

/// `.decal.ron`: v1/v2 → v3 moves the version (new fields have defaults), so
/// upgrading is the loader's text migration, which keeps comments and formatting (ADR-3255).
fn migrate_decal(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    let migrated = regia_vfx::decal_format::migrate_decal_ron(text).map_err(|e| e.to_string())?;
    regia_vfx::decal_format::DecalPrefab::from_ron(&migrated).map_err(|e| e.to_string())?;
    Ok(migrated.into_bytes())
}

/// The version of a cooked world index (`Ok(None)` for another cooked type).
fn read_world_index_version(bytes: &[u8]) -> Result<Option<u32>, String> {
    regia_world::format::world_index_file_version(bytes).map_err(|e| e.to_string())
}

/// Cooked world index v1 → v2: every HLOD proxy without an atlas and the atlas setting off
/// (ADR-2483); the cooked header kept.
fn migrate_world_index(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    regia_world::format::migrate_world_index_file(bytes)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "not an older world index".to_string())
}

/// `.fgiv` baked GI volumes: v1-v3 gain an absent split section (one unsplit volume, ADR-2618) and
/// v4 parts' bricks the v5 crop fields (no crop, ADR-3237); brick flags, the emitter list and the
/// sections are kept.
fn migrate_baked_gi(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    regia_asset::baked_gi_split::migrate_baked_gi_file(bytes)
}

/// `.ragdoll.ron` v1 → v2 only adds `shape: Capsule` to every bone (ADR-2133 § 6), v2 → v3 only bumps the
/// version (no `active` block: zero gains, ADR-2498), and so does v3 → v4 (no `recovery` block, ADR-2963): load (v1 bones
/// are capsules) → save.
fn migrate_ragdoll(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    regia::framework::ragdoll::file::migrate_ragdoll(text)
        .map(String::into_bytes)
        .map_err(|e| e.to_string())
}

/// Cooked motion databases (`.fckd` with a `regia_animation::MotionDatabase` payload): v1 and v2
/// load without the optional arrays, so upgrading is decode → re-encode at v3 with the clip
/// speeds added and the cooked header kept (ADR-7817).
fn migrate_cooked_motion_database(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    regia_animation::cooked::migrate_cooked_motion_database_file(bytes)
}

/// `.motiondb.ron` v1 → v2 only rewrites the version (the contact settings and the new weights are
/// optional with defaults that leave the database unchanged, ADR-2858).
fn migrate_motiondb(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    regia_animation::motion_matching::asset::migrate_motiondb(text)
        .map(String::into_bytes)
        .map_err(|e| e.to_string())
}

/// `.retarget.ron` v1 → v2 adds the `ik_goals` list, empty, so the setup bakes what it baked before
/// (ADR-8476).
fn migrate_retarget(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    regia_animation::migrate_retarget(text)
        .map(String::into_bytes)
        .map_err(|e| e.to_string())
}

/// `.regiademo`: the version sits in the nested header, so the generic RON reader cannot see it.
fn demo_file_version(bytes: &[u8]) -> Result<Option<u32>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    regia::framework::demo::demo_file_version(text)
}

/// `.regiademo` v1 → v2 only adds the (empty) timestamps: load (which migrates) → save.
fn migrate_demo(bytes: &[u8], _from: u32) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8: {e}"))?;
    let demo = regia::framework::demo::DemoRecording::from_ron(text).map_err(|e| e.to_string())?;
    demo.to_ron().map(String::into_bytes).map_err(|e| e.to_string())
}

/// One file found by the scan, with its on-disk version.
struct FilePlan {
    /// Project-relative path.
    path: PathBuf,
    /// Format the file belongs to.
    format: &'static str,
    /// Version read from the file.
    found: u32,
    /// The current version of the format.
    current: u32,
}

impl FilePlan {
    /// True when the file must be migrated (`found < current`).
    fn needs_migration(&self) -> bool {
        self.found < self.current
    }

    fn status(&self) -> String {
        if self.found < self.current {
            format!("v{} -> v{}", self.found, self.current)
        } else if self.found > self.current {
            format!(
                "v{} (newer than engine v{}; cannot downgrade)",
                self.found, self.current
            )
        } else {
            "up to date".to_string()
        }
    }
}

/// Reads the version of `path` using `format`; `Ok(None)` when a [`VersionSource::Probe`] says
/// the file is not of this format.
fn read_version(format: &Format, bytes: &[u8]) -> Result<Option<u32>, String> {
    match &format.source {
        VersionSource::Ron => read_ron_version(bytes).map(Some),
        VersionSource::Binary(magic) => read_binary_version(bytes, magic).map(Some),
        VersionSource::ImplicitV1 => read_implicit_v1(bytes).map(Some),
        VersionSource::Probe(probe) => probe(bytes),
    }
}

/// True when the walker should descend into `entry`. Skips VCS metadata, cargo output, engine
/// trees (a vendored release or checkout holds the engine's own test fixtures, never project
/// content; spec 94 § 4.1) and the regenerable package output trees: `regia package` stages
/// real frozen-format content under `build/` and ships the bundle from `dist/<platform>/`
/// (`commands/package.rs`), but both are regenerated by the next package run, so migrating them
/// in place would rewrite shipped bytes and drop `.bak` files into the bundle. The deliberate
/// `.regia/cooked` descent (cooked-cache migration) is untouched. A staged copy an interrupted
/// `--write` left (`.<name>.<pid>.stage`, ADR-8861) is never project content either.
fn walkable(entry: &walkdir::DirEntry) -> bool {
    let name = entry.file_name().to_string_lossy();
    !name.starts_with(".git")
        && name != "target"
        && !is_stage_dir(entry)
        && (entry.depth() == 0 || !entry.file_type().is_dir() || !matches!(name.as_ref(), "build" | "dist"))
        && (entry.depth() == 0 || !entry.file_type().is_dir() || !distribution::is_engine_tree(entry.path()))
}

/// Finds every frozen-format file under `project` and records its version.
fn plan_with(project: &Path, formats: &[Format]) -> anyhow::Result<Vec<FilePlan>> {
    let mut files = Vec::new();
    let walker = walkdir::WalkDir::new(project).into_iter().filter_entry(walkable);
    for entry in walker {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy();
        let candidates: Vec<&Format> = formats
            .iter()
            .filter(|f| f.suffixes.iter().any(|s| owns_name(s, &name)))
            .collect();
        if candidates.is_empty() {
            continue;
        }
        let bytes = std::fs::read(entry.path())?;
        // Formats can share a suffix (`.fckd`): the first whose probe recognizes the file owns it.
        let mut owner = None;
        for format in candidates {
            match read_version(format, &bytes) {
                Ok(None) => {}
                other => {
                    owner = Some((format, other));
                    break;
                }
            }
        }
        let Some((format, version)) = owner else {
            continue;
        };
        match version {
            Ok(None) => {}
            Ok(Some(found)) => files.push(FilePlan {
                path: entry
                    .path()
                    .strip_prefix(project)
                    .unwrap_or_else(|_| entry.path())
                    .to_path_buf(),
                format: format.name,
                found,
                current: format.current,
            }),
            Err(err) => {
                tracing::warn!(target: "regia_cli", path = %entry.path().display(), "{err}: skipped");
            }
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

/// Adds a plan entry per scene file (`scene-level`, `scene-prefab` entries that are not newer than
/// the engine) and embedded component format (`post-volume`, `physical-camera`) the file holds
/// (`super::upgrade_components`, ADR-2323). The entries follow the file's own entry, so `--write`
/// migrates the file first and then its components.
fn plan_embedded(project: &Path, files: &mut Vec<FilePlan>) {
    let scenes: Vec<PathBuf> = files
        .iter()
        .filter(|f| matches!(f.format, "scene-level" | "scene-prefab") && f.found <= f.current)
        .map(|f| f.path.clone())
        .collect();
    for rel in scenes {
        let path = project.join(&rel);
        let found = std::fs::read(&path)
            .map_err(|e| e.to_string())
            .and_then(|bytes| super::upgrade_components::embedded_versions(&bytes));
        match found {
            Ok(found) => {
                for (format, found, current) in found {
                    files.push(FilePlan {
                        path: rel.clone(),
                        format,
                        found,
                        current,
                    });
                }
            }
            Err(err) => {
                tracing::warn!(target: "regia_cli", path = %path.display(), "embedded components: {err}: skipped");
            }
        }
    }
    // Stable: a file's own entry stays ahead of its components.
    files.sort_by(|a, b| a.path.cmp(&b.path));
}

/// Adds a plan row for each embedded component format found in a level directory (spec 83
/// § 3.3), keyed by the directory's path. A directory newer than the engine is skipped (it is
/// reported as newer and never touched); one that does not read is logged and skipped.
fn plan_embedded_dirs(project: &Path, dirs: &[LevelDirPlan], files: &mut Vec<FilePlan>) {
    for dir in dirs
        .iter()
        .filter(|d| d.scene_newest <= regia_scene::format::SCENE_FORMAT_VERSION)
    {
        let root = project.join(&dir.path);
        match super::upgrade_components::level_dir_embedded_versions(&root) {
            Ok(found) => {
                for (format, found, current) in found {
                    files.push(FilePlan {
                        path: dir.path.clone(),
                        format,
                        found,
                        current,
                    });
                }
            }
            Err(err) => {
                tracing::warn!(target: "regia_cli", path = %root.display(), "embedded components: {err}: skipped");
            }
        }
    }
}

/// Run the command: dry run (default) or in-place migration with `--write`.
pub fn run(args: &UpgradeProjectArgs) -> anyhow::Result<()> {
    run_with(args, &formats())
}

/// The engine part of an upgrade (spec 94): the project's requirement, its pin, the pin rewrite
/// for `--engine` and the line to stamp.
struct EnginePlan {
    /// The `project` section of the JSON document (`null` without a manifest).
    project: Value,
    /// The `pin` section (`null` without a manifest).
    pin: Value,
    /// Manifest rewrites for `--engine`.
    edits: Vec<PinEdit>,
    /// The line `--write` stamps in `Regia.toml`.
    stamp: Option<ReleaseLine>,
    /// Findings; an error refuses the whole run before anything is written.
    problems: Vec<Problem>,
    /// The `--engine` path's directory, whose `Cargo.lock` `--write` seeds into a project without one.
    lock_source: Option<PathBuf>,
}

/// Checks a `--engine` source against this engine's version.
fn check_source(project: &Path, source: &EngineSource, problems: &mut Vec<Problem>) {
    let engine = engine_version::CURRENT;
    match source {
        EngineSource::Path(dir) => {
            let dir = if dir.is_absolute() || distribution::is_absolute_text(&distribution::slash_path(dir)) {
                dir.clone()
            } else {
                project.join(dir)
            };
            match distribution::engine_dir_version(&dir) {
                Some(found) if found == engine => {}
                Some(found) => problems.push(Problem::error(
                    "engine_source_version_mismatch",
                    format!(
                        "{} is engine {found}; this regia is {engine}: run the `regia` of the engine you upgrade to",
                        dir.display()
                    ),
                )),
                None => problems.push(Problem::error(
                    "engine_source_missing",
                    format!(
                        "{} holds no engine (no regia-engine.release.ron and no workspace version); copy the release there first",
                        dir.display()
                    ),
                )),
            }
        }
        EngineSource::Git { tag, .. } => match engine_version::EngineVersion::parse(tag) {
            Ok(found) if found == engine => {}
            // The same refusal as a path or a registry source: a tag that names another engine
            // would rewrite the pins to it, a downgrade included.
            Ok(found) => problems.push(Problem::error(
                "engine_source_version_mismatch",
                format!(
                    "tag `{tag}` is engine {found}; this regia is {engine}: run the `regia` of the engine you upgrade to"
                ),
            )),
            Err(_) => problems.push(Problem::warning(
                "engine_tag_unverified",
                format!("tag `{tag}` names no engine version; the command cannot fetch it to check"),
            )),
        },
        EngineSource::Registry { version, .. } => {
            if *version != engine {
                problems.push(Problem::error(
                    "engine_source_version_mismatch",
                    format!("registry version {version} is not this regia's engine {engine}"),
                ));
            }
        }
    }
}

/// Builds the [`EnginePlan`]. A directory without a project manifest has no engine part (the
/// migrators still run), unless `--engine` asks for one.
fn engine_plan(args: &UpgradeProjectArgs) -> EnginePlan {
    let project = &args.project;
    let engine = engine_version::CURRENT;
    let mut plan = EnginePlan {
        project: Value::Null,
        pin: Value::Null,
        edits: Vec::new(),
        stamp: None,
        problems: Vec::new(),
        lock_source: None,
    };
    // The requirement lives in the project manifest; a quick project (one crate, no manifest)
    // still has a pin.
    if regia_core::brand::is_project_root(project) {
        match distribution::read_requirement(project) {
            Ok(requirement) => {
                let status = engine_version::assess(requirement.as_ref(), engine);
                plan.project = super::engine::requirement_json(requirement.as_ref(), &status);
                plan.problems.extend(super::engine::status_problem(&status));
                plan.stamp = status.stamp();
            }
            Err(err) => plan.problems.push(err.into()),
        }
    }
    let source = args
        .engine
        .as_deref()
        .map(|text| EngineSource::parse(text, engine));
    match distribution::read_pin(project) {
        Ok(pin) => {
            let (mut doc, found) = super::engine::pin_json(project, &pin, engine);
            // A stale pin is the reason for `--engine`; with it, the stale warning is moot.
            plan.problems.extend(
                found
                    .into_iter()
                    .filter(|p| source.is_none() || p.code != "engine_pin_stale"),
            );
            if source.is_some() && pin.sites.is_empty() {
                plan.problems.push(Problem::warning(
                    "engine_pin_empty",
                    format!("{} has no engine dependency to move", project.display()),
                ));
            }
            doc["target"] = json!(
                source
                    .as_ref()
                    .and_then(|s| s.as_ref().ok())
                    .map(ToString::to_string)
            );
            plan.pin = doc;
        }
        Err(err) => {
            let mut problem: Problem = err.into();
            if source.is_none() {
                problem.severity = Severity::Warning;
            }
            plan.problems.push(problem);
        }
    }
    match source {
        None => {}
        Some(Err(err)) => plan.problems.push(err.into()),
        Some(Ok(source)) => {
            check_source(project, &source, &mut plan.problems);
            if let Some(dir) = source.local_dir(project) {
                let drift = distribution::lock_drift(project, &dir);
                plan.pin["lock_drift"] = json!(drift);
                match drift {
                    Some(0) => {}
                    Some(drift) => plan.problems.push(Problem::warning(
                        "engine_lock_differs",
                        format!(
                            "{drift} third-party package/version row(s) in the project's Cargo.lock are not the engine's \
                             tested versions; copy {}/Cargo.lock over it to take them (the game's own \
                             dependencies then re-resolve)",
                            dir.display()
                        ),
                    )),
                    None => plan.lock_source = Some(dir),
                }
            }
            match distribution::rewrite_pin(project, &source) {
                Ok(edits) => {
                    plan.pin["edits"] = json!(
                        edits
                            .iter()
                            .map(|e| json!({ "file": e.file, "changed_lines": e.changed_lines }))
                            .collect::<Vec<_>>()
                    );
                    plan.edits = edits;
                }
                Err(err) => plan.problems.push(err.into()),
            }
        }
    }
    plan
}

/// The `--json` document of `upgrade-project`.
fn upgrade_json(
    args: &UpgradeProjectArgs,
    plan: &EnginePlan,
    files: &[FilePlan],
    moves: &[(PathBuf, PathBuf)],
    build_scripts: &[BuildScriptPlan],
) -> Value {
    let ok = plan.problems.iter().all(|p| p.severity == Severity::Warning);
    json!({
        "schema": super::engine::JSON_SCHEMA,
        "kind": "regia_upgrade_project",
        "ok": ok,
        "write": args.write,
        "engine": {
            "version": engine_version::CURRENT.to_string(),
            "line": engine_version::CURRENT.line().to_string(),
        },
        "project": plan.project,
        "pin": plan.pin,
        "stamp": plan.stamp.map(|l| l.to_string()),
        "files": files.iter().map(|f| json!({
            "path": distribution::slash_path(&f.path),
            "format": f.format,
            "found": f.found,
            "current": f.current,
            "status": match f.found.cmp(&f.current) {
                std::cmp::Ordering::Less => "migrate",
                std::cmp::Ordering::Greater => "newer",
                std::cmp::Ordering::Equal => "current",
            },
        })).collect::<Vec<_>>(),
        "renames": moves.iter().map(|(from, to)| json!({
            "from": from.display().to_string(),
            "to": to.display().to_string(),
        })).collect::<Vec<_>>(),
        "problems": plan.problems.iter().map(Problem::to_json).collect::<Vec<_>>(),
        "build_scripts": build_scripts.iter().map(|b| json!({
            "path": b.script(),
            "manifest": b.manifest(),
            "status": b.status(),
        })).collect::<Vec<_>>(),
    })
}

/// Writes `text` over the project file `rel` with a `.bak` of the original first. Returns the
/// backup's project-relative name (`.bak.2`, … when an earlier run left a `.bak`).
fn replace_with_backup(project: &Path, rel: &str, text: &str) -> std::io::Result<String> {
    let path = project.join(rel);
    let original = std::fs::read(&path)?;
    let backup = free_backup(project.join(format!("{rel}.bak")));
    write_atomic(&backup, &original)?;
    write_atomic(&path, text.as_bytes())?;
    Ok(backup_label(project, &backup))
}

/// The project-relative name of a backup, for the messages (`/` separators).
fn backup_label(project: &Path, backup: &Path) -> String {
    backup
        .strip_prefix(project)
        .unwrap_or(backup)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Directories a project backup leaves out at the project root. Anywhere below the root, a
/// directory holding a `CACHEDIR.TAG` and every subfolder of a project state directory (`.regia/`,
/// `.ferrite/`) is left out too, but the state directory's own files are kept (see
/// [`backup_skips`]). The editor's Duplicate project leaves out these and the whole state
/// directory (`regia_editor::project::COPY_SKIP_ROOT_DIRS`): a copy starts with fresh state, a
/// backup must hold what `--write` rewrites.
const BACKUP_SKIP_ROOT_DIRS: &[&str] = &["target", "build", "dist", ".git"];

/// What [`backup_project`] copied.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct BackupReport {
    /// The dated backup folder.
    path: PathBuf,
    /// Files copied.
    files: u64,
    /// Bytes copied.
    bytes: u64,
    /// What was left out, project-relative: skipped directories (`name/`) and symbolic links.
    skipped: Vec<String>,
}

/// Seconds since the Unix epoch now (0 before it).
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `<project name>-backup-YYYYMMDD-HHMMSS` (UTC) for `secs` since the Unix epoch.
fn backup_folder_name(project: &Path, secs: u64) -> String {
    let name = project
        .canonicalize()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "project".to_string());
    let (y, mo, d, h, mi, se) = regia_core::log::civil_date_time(secs);
    format!("{name}-backup-{y:04}{mo:02}{d:02}-{h:02}{mi:02}{se:02}")
}

/// Whether `name` is a project state directory name (`.regia`, or the legacy `.ferrite`).
fn is_state_dir_name(name: &str) -> bool {
    name == regia_core::brand::PROJECT_STATE_DIR || name == regia_core::brand::LEGACY_PROJECT_STATE_DIR
}

/// Whether the backup leaves the directory `rel` (project-relative) out: a root entry of
/// [`BACKUP_SKIP_ROOT_DIRS`], a folder holding a `CACHEDIR.TAG`, or a subfolder of a project
/// state directory (cooked output, caches, the asset database, autosaves, version history, the
/// agent endpoint: regenerable or session state). The state directory itself is entered, so its
/// own files (the project-local editor layouts and views `--write` migrates, `agent.toml`) are
/// in the backup, under the name they have before the brand move (review A2, ADR-8435).
fn backup_skips(dir: &Path, rel: &Path) -> bool {
    let name = rel.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let in_state_dir = rel
        .parent()
        .and_then(Path::file_name)
        .and_then(|n| n.to_str())
        .is_some_and(is_state_dir_name);
    (rel.components().count() == 1 && BACKUP_SKIP_ROOT_DIRS.contains(&name))
        || in_state_dir
        || dir.join("CACHEDIR.TAG").is_file()
}

/// Copies `project`'s source files into `staging` (created), counting into `report` (see
/// [`backup_skips`]); symbolic links are listed as skipped, never followed.
fn copy_backup_tree(project: &Path, staging: &Path, report: &mut BackupReport) -> std::io::Result<()> {
    std::fs::create_dir_all(staging)?;
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        let mut entries: Vec<std::fs::DirEntry> =
            std::fs::read_dir(project.join(&rel))?.collect::<Result<_, _>>()?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let child = rel.join(entry.file_name());
            let shown = child.to_string_lossy().replace('\\', "/");
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                report.skipped.push(shown);
            } else if kind.is_dir() {
                if backup_skips(&entry.path(), &child) {
                    report.skipped.push(format!("{shown}/"));
                } else {
                    std::fs::create_dir_all(staging.join(&child))?;
                    stack.push(child);
                }
            } else {
                report.bytes += std::fs::copy(entry.path(), staging.join(&child))?;
                report.files += 1;
            }
        }
    }
    Ok(())
}

/// Copies the project's source files to a new dated folder under `dir` (`-2`, `-3`, ... when the
/// name is taken), through a staging folder renamed into place. Refuses a `dir` inside the project
/// (the migrators would walk the backup too).
fn backup_project(project: &Path, dir: &Path, secs: u64) -> anyhow::Result<BackupReport> {
    std::fs::create_dir_all(dir)
        .map_err(|e| anyhow::anyhow!("backup_failed: cannot create {}: {e}", dir.display()))?;
    let project_abs = project.canonicalize()?;
    let dir_abs = dir.canonicalize()?;
    if dir_abs.starts_with(&project_abs) {
        anyhow::bail!(
            "backup_inside_project: {} is inside the project; pass a folder outside it (nothing written)",
            dir.display()
        );
    }
    let base = backup_folder_name(project, secs);
    let path = (1u32..=1000)
        .map(|n| {
            if n == 1 {
                dir_abs.join(&base)
            } else {
                dir_abs.join(format!("{base}-{n}"))
            }
        })
        .find(|p| !p.exists())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "backup_failed: every backup name under {} is taken",
                dir.display()
            )
        })?;
    let staging = dir_abs.join(format!(".{base}.partial-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    let mut report = BackupReport {
        path: path.clone(),
        ..BackupReport::default()
    };
    let copied =
        copy_backup_tree(&project_abs, &staging, &mut report).and_then(|()| std::fs::rename(&staging, &path));
    if let Err(err) = copied {
        let _ = std::fs::remove_dir_all(&staging);
        anyhow::bail!(
            "backup_failed: copying the project to {}: {err} (nothing migrated)",
            path.display()
        );
    }
    report.skipped.sort();
    Ok(report)
}

/// The `backup` section of the JSON document: what `--write` copied, or (dry run) where it would.
fn backup_json(args: &UpgradeProjectArgs, report: Option<&BackupReport>) -> Value {
    match (report, &args.backup) {
        (Some(report), _) => json!({
            "written": true,
            "path": distribution::slash_path(&report.path),
            "files": report.files,
            "bytes": report.bytes,
            "skipped": report.skipped,
        }),
        (None, Some(dir)) => json!({ "written": false, "dir": distribution::slash_path(dir) }),
        (None, None) => Value::Null,
    }
}

/// [`run`] against an explicit format table (so tests can inject a format with a real migrator).
fn run_with(args: &UpgradeProjectArgs, formats: &[Format]) -> anyhow::Result<()> {
    let plan = engine_plan(args);
    let mut files = plan_with(&args.project, formats)?;
    plan_embedded(&args.project, &mut files);
    let level_dirs = plan_level_dirs(&args.project)?;
    plan_embedded_dirs(&args.project, &level_dirs, &mut files);
    let need = files.iter().filter(|f| f.needs_migration()).count();
    let moves = brand_moves(&args.project, &brand_names());
    let gitignore = gitignore_addition(&args.project, &moves);
    let build_scripts = plan_build_scripts(&args.project);

    for problem in &plan.problems {
        eprintln!(
            "  {} {}: {}",
            problem.severity.as_str(),
            problem.code,
            problem.message
        );
    }
    let errors: Vec<&str> = plan
        .problems
        .iter()
        .filter(|p| p.severity == Severity::Error)
        .map(|p| p.code)
        .collect();
    if !errors.is_empty() {
        if args.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&upgrade_json(args, &plan, &files, &moves, &build_scripts))?
            );
        }
        anyhow::bail!("upgrade-project refused (nothing written): {}", errors.join(", "));
    }
    let notes = plan.project["release_notes"].as_array();
    for note in notes.into_iter().flatten().filter_map(Value::as_str) {
        eprintln!("  release notes to read: {note}");
    }
    clean_stale_stage_dirs(&args.project, args.write);

    if !args.write {
        for file in &files {
            eprintln!("  {} ({}): {}", file.path.display(), file.format, file.status());
        }
        for dir in &level_dirs {
            eprintln!("  {} (level-dir): {}", dir.path.display(), dir.status());
        }
        for edit in &plan.edits {
            eprintln!(
                "  {}: {} engine dependency line(s) would point at the new source",
                edit.file, edit.changed_lines
            );
        }
        if let Some(line) = plan.stamp {
            eprintln!(
                "  {}: [project] engine would be \"{line}\"",
                regia_core::brand::MANIFEST_FILE
            );
        }
        for (legacy, current) in &moves {
            eprintln!(
                "  {} -> {}: name from before the rename (ADR-1388; still read for one release)",
                legacy.display(),
                current.display()
            );
        }
        if let Some(line) = &gitignore {
            eprintln!("  .gitignore: {line} would be added (the renamed state directory stays ignored)");
        }
        print_build_script_plans(&build_scripts);
        if let Some(dir) = &args.backup {
            eprintln!(
                "  --write would first copy the project's source files to {}/{}",
                dir.display(),
                backup_folder_name(&args.project, unix_now())
            );
        }
        eprintln!(
            "upgrade-project: {} file(s), {} need migration (run with --write to apply)",
            files.len(),
            need
        );
        if args.json {
            let mut doc = upgrade_json(args, &plan, &files, &moves, &build_scripts);
            if args.backup.is_some() {
                doc["backup"] = backup_json(args, None);
            }
            println!("{}", serde_json::to_string_pretty(&doc)?);
        }
        return Ok(());
    }
    // The whole-project backup before anything is written (ADR-7928).
    let backup = match &args.backup {
        Some(dir) => {
            let report = backup_project(&args.project, dir, unix_now())?;
            eprintln!(
                "  backup: copied {} file(s) to {} (left out: {})",
                report.files,
                report.path.display(),
                if report.skipped.is_empty() {
                    "nothing".to_string()
                } else {
                    report.skipped.join(", ")
                }
            );
            Some(report)
        }
        None => None,
    };

    let (migrated, backed_up) = apply_migrations(args, formats, &files, &level_dirs)?;
    let newer = files.iter().filter(|f| f.found > f.current).count();
    for file in files.iter().filter(|f| f.found > f.current) {
        eprintln!(
            "  {} ({}): v{} is newer than engine v{}; skipped",
            file.path.display(),
            file.format,
            file.found,
            file.current
        );
    }
    apply_brand_moves(&moves)?;
    for (legacy, current) in &moves {
        eprintln!("  renamed {} -> {}", legacy.display(), current.display());
    }
    if let Some(line) = &gitignore {
        let path = args.project.join(".gitignore");
        let mut text = std::fs::read_to_string(&path)?;
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(line);
        text.push('\n');
        write_atomic(&path, text.as_bytes())?;
        eprintln!("  .gitignore: added {line}");
    }
    // The engine pin and line last, after the manifest's possible rename above (spec 94).
    for edit in &plan.edits {
        let backup = replace_with_backup(&args.project, &edit.file, &edit.text)?;
        eprintln!(
            "  {}: {} engine dependency line(s) rewritten (backup: {backup})",
            edit.file, edit.changed_lines
        );
    }
    if let Some(dir) = &plan.lock_source
        && distribution::seed_lockfile(&args.project, dir)?
    {
        eprintln!("  Cargo.lock: seeded from {}", dir.display());
    }
    if let Some(line) = plan.stamp
        && let Some(manifest) = regia_core::brand::project_manifest(&args.project)
    {
        let text = std::fs::read_to_string(&manifest)?;
        if let Some(stamped) = distribution::stamp_engine_line(&text, line)
            && stamped != text
        {
            let rel = manifest
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let backup = replace_with_backup(&args.project, &rel, &stamped)?;
            eprintln!("  {rel}: [project] engine = \"{line}\" (backup: {backup})");
        }
    }
    // The Windows build scripts after the engine dependency edits, which rewrite the same
    // manifests (ADR-2645).
    let scripts = apply_build_script_plans(&args.project, &build_scripts)?;
    if scripts > 0 {
        eprintln!("  {scripts} game executable(s) got the Windows build script or its dependencies");
    }
    eprintln!(
        "upgrade-project: migrated {migrated} file(s), backed up {backed_up}, {newer} newer version(s) skipped"
    );
    if args.json {
        let mut doc = upgrade_json(args, &plan, &files, &moves, &build_scripts);
        if args.backup.is_some() {
            doc["backup"] = backup_json(args, backup.as_ref());
        }
        println!("{}", serde_json::to_string_pretty(&doc)?);
    }
    Ok(())
}

/// Project-root names from before the rename to Regia Engine (ADR-1388) that `--write` moves to
/// the current names, as `(legacy, current)`: the manifest and the project state directory.
fn brand_names() -> [(&'static str, &'static str); 2] {
    [
        (
            regia_core::brand::LEGACY_MANIFEST_FILE,
            regia_core::brand::MANIFEST_FILE,
        ),
        (
            regia_core::brand::LEGACY_PROJECT_STATE_DIR,
            regia_core::brand::PROJECT_STATE_DIR,
        ),
    ]
}

/// The project-root entries to move: a legacy name that exists while its current name does not
/// (an existing current entry is never overwritten). Empty before the rename, when the names agree.
fn brand_moves(project: &Path, names: &[(&str, &str)]) -> Vec<(PathBuf, PathBuf)> {
    names
        .iter()
        .filter(|(legacy, current)| legacy != current)
        .map(|(legacy, current)| (project.join(legacy), project.join(current)))
        .filter(|(legacy, current)| legacy.exists() && !current.exists())
        .collect()
}

/// The `.gitignore` line `--write` appends when it upgrades the project's names: the line that
/// ignored the legacy state directory, spelled with the current name, so the cook cache and the
/// agent's bearer token stay out of git once the state directory has (or gets) the new name
/// (ADR-1429). `None` when nothing moves or the project's `.gitignore` needs no line (see
/// `gitignore_line`).
fn gitignore_addition(project: &Path, moves: &[(PathBuf, PathBuf)]) -> Option<String> {
    if moves.is_empty() {
        return None;
    }
    let legacy = regia_core::brand::LEGACY_PROJECT_STATE_DIR;
    let text = std::fs::read_to_string(project.join(".gitignore")).ok()?;
    gitignore_line(&text, legacy, regia_core::brand::PROJECT_STATE_DIR)
}

/// The `legacy` ignore line of `text` rewritten for `current` (`/.old/` → `/.new/`); `None` when
/// no line names `legacy` or a line already names `current`.
fn gitignore_line(text: &str, legacy: &str, current: &str) -> Option<String> {
    let names = |line: &str, dir: &str| line.trim().trim_start_matches('/').trim_end_matches('/') == dir;
    if text.lines().any(|line| names(line, current)) {
        return None;
    }
    let line = text.lines().find(|line| names(line, legacy))?;
    Some(line.trim().replacen(legacy, current, 1))
}

/// Renames each `(legacy, current)` pair (one rename each, on the same volume).
fn apply_brand_moves(moves: &[(PathBuf, PathBuf)]) -> std::io::Result<()> {
    for (legacy, current) in moves {
        std::fs::rename(legacy, current)?;
    }
    Ok(())
}

/// One level directory (`<name>.level.d/`, format `level-dir`, spec 37 § 3.5) and the oldest
/// versions found in it. Upgrading rewrites every file at the latest versions and keeps the
/// directory form.
struct LevelDirPlan {
    /// Path relative to the project.
    path: PathBuf,
    /// Oldest `LEVEL_DIR_FORMAT_VERSION` among its files.
    dir_version: u32,
    /// Oldest scene format version among its files.
    scene_version: u32,
    /// Newest scene format version among its files.
    scene_newest: u32,
}

impl LevelDirPlan {
    fn needs_migration(&self) -> bool {
        self.scene_newest <= regia_scene::format::SCENE_FORMAT_VERSION
            && (self.dir_version < regia_scene::level_dir::LEVEL_DIR_FORMAT_VERSION
                || self.scene_version < regia_scene::format::SCENE_FORMAT_VERSION)
    }

    fn status(&self) -> String {
        let scene = regia_scene::format::SCENE_FORMAT_VERSION;
        if self.scene_newest > scene {
            format!(
                "scene v{} (newer than engine v{scene}; cannot downgrade)",
                self.scene_newest
            )
        } else if self.needs_migration() {
            format!(
                "dir v{} scene v{}..v{} -> dir v{} scene v{scene}",
                self.dir_version,
                self.scene_version,
                self.scene_newest,
                regia_scene::level_dir::LEVEL_DIR_FORMAT_VERSION
            )
        } else {
            "up to date".to_string()
        }
    }
}

/// Whether `entry` is a staging directory of `--write` (`.<name>.<pid>.stage`, ADR-8811): its
/// copy of a level directory is never project content (ADR-8861).
fn is_stage_dir(entry: &walkdir::DirEntry) -> bool {
    entry.depth() > 0
        && entry.file_type().is_dir()
        && regia_scene::level_dir::is_upgrade_stage_dir_name(&entry.file_name().to_string_lossy())
}

/// Staging directories an interrupted `--write` left under `project` (the scan of
/// [`plan_level_dirs`]; a staging directory is not searched inside), sorted.
fn stale_stage_dirs(project: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut walker = walkdir::WalkDir::new(project).into_iter();
    while let Some(entry) = walker.next() {
        let Ok(entry) = entry else {
            continue;
        };
        let name = entry.file_name().to_string_lossy();
        if entry.depth() > 0
            && entry.file_type().is_dir()
            && (name.starts_with(".git")
                || name == "target"
                || matches!(name.as_ref(), "build" | "dist")
                || distribution::is_engine_tree(entry.path()))
        {
            walker.skip_current_dir();
            continue;
        }
        if is_stage_dir(&entry) {
            out.push(entry.path().to_path_buf());
            walker.skip_current_dir();
        }
    }
    out.sort();
    out
}

/// Removes the staging directories an interrupted `--write` left (ADR-8861): a run that was killed
/// between staging a level directory and swapping it in leaves `.<name>.<pid>.stage/<name>`, a
/// second copy of the level with the same GUIDs. The dry run (`write == false`) only says what it
/// would remove. Returns the printed lines.
fn clean_stale_stage_dirs(project: &Path, write: bool) -> Vec<String> {
    let mut lines = Vec::new();
    for dir in stale_stage_dirs(project) {
        let rel = dir.strip_prefix(project).unwrap_or(&dir).display().to_string();
        let line = if !write {
            format!("  {rel}: staging directory left by an interrupted --write; --write would remove it")
        } else {
            match std::fs::remove_dir_all(&dir) {
                Ok(()) => format!("  removed {rel}: staging directory left by an interrupted --write"),
                Err(err) => format!("  warning stage_dir_not_removed: {rel}: {err}"),
            }
        };
        eprintln!("{line}");
        lines.push(line);
    }
    lines
}

/// Finds every level directory under `project`, skipping what [`walkable`] skips (a vendored
/// engine's own levels, the regenerable package output trees and `--write` staging directories
/// are never project content).
fn plan_level_dirs(project: &Path) -> anyhow::Result<Vec<LevelDirPlan>> {
    let mut out = Vec::new();
    let walker = walkdir::WalkDir::new(project).into_iter().filter_entry(walkable);
    for entry in walker {
        let entry = entry?;
        if !entry.file_type().is_dir()
            || !entry
                .file_name()
                .to_string_lossy()
                .ends_with(regia_scene::level_dir::LEVEL_DIR_SUFFIX)
        {
            continue;
        }
        match regia_scene::level_dir::level_dir_versions(entry.path()) {
            Ok((dir_version, scene_version, scene_newest)) => out.push(LevelDirPlan {
                path: entry
                    .path()
                    .strip_prefix(project)
                    .unwrap_or_else(|_| entry.path())
                    .to_path_buf(),
                dir_version,
                scene_version,
                scene_newest,
            }),
            Err(err) => {
                anyhow::bail!(
                    "level_dir_unreadable: cannot inspect {}: {err}; nothing written",
                    entry.path().display()
                );
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// One file `--write` migrates: its bytes before and after every migration of this run (a scene
/// migrates, then its embedded components).
struct StagedFile {
    /// Project-relative path.
    rel: PathBuf,
    /// The bytes on disk before the run (the backup).
    original: Vec<u8>,
    /// The migrated bytes.
    output: Vec<u8>,
    /// The `migrated ...` lines printed once the file is in place.
    notes: Vec<String>,
}

/// One level directory `--write` migrates: a copy of it, migrated, at `<parent>/.<name>.<pid>.stage/<name>`
/// (the copy keeps the directory's name).
struct StagedDir {
    /// Project-relative path.
    rel: PathBuf,
    /// The staging directory that holds the copy.
    stage: PathBuf,
    /// The `migrated ...` lines printed once the directory is in place.
    notes: Vec<String>,
}

impl StagedDir {
    /// The migrated copy.
    fn copy(&self) -> PathBuf {
        self.stage.join(self.rel.file_name().unwrap_or_default())
    }
}

/// The `--write` migrations, in two phases (ADR-8811). Phase 1 migrates every file (in memory) and
/// every level directory (in a staged copy) and collects every error; the project is not touched.
/// Any error: the staged copies are removed and the run fails listing every error. Phase 2 writes
/// each file's migrated bytes to a staged sibling, then every backup, then renames each staged file
/// and directory in. A failure while staging or writing backups removes what this run wrote, so
/// the project is unchanged. Returns `(migrated, backed_up)`.
fn apply_migrations(
    args: &UpgradeProjectArgs,
    formats: &[Format],
    files: &[FilePlan],
    level_dirs: &[LevelDirPlan],
) -> anyhow::Result<(usize, usize)> {
    let mut errors: Vec<String> = Vec::new();
    let mut staged_files: Vec<StagedFile> = Vec::new();
    for file in files.iter().filter(|f| f.needs_migration()) {
        let path = args.project.join(&file.path);
        // Components of a level directory migrate with the directory itself, below.
        if path.is_dir() {
            continue;
        }
        // Components inside a scene file (post-volume, physical-camera) migrate through
        // `upgrade_components`; every other format through its registry entry.
        let embedded = super::upgrade_components::embedded_format(file.format);
        let native = formats.iter().find(|f| f.name == file.format);
        if embedded.is_none() && native.is_none() {
            continue;
        }
        // A file migrated twice in one run (the scene, then its components) reads the first
        // migration's output and keeps one backup: the original bytes.
        let at = staged_files.iter().position(|s| s.rel == file.path);
        let input = match at.and_then(|i| staged_files.get(i)) {
            Some(staged) => staged.output.clone(),
            None => match std::fs::read(&path) {
                Ok(bytes) => bytes,
                Err(err) => {
                    errors.push(format!("{}: {err}", file.path.display()));
                    continue;
                }
            },
        };
        let output = match (&embedded, native) {
            (Some(component), _) => super::upgrade_components::migrate_embedded(component, &input),
            (None, Some(format)) => (format.migrate)(&input, file.found),
            (None, None) => continue,
        };
        let output = match output {
            Ok(output) => output,
            Err(err) => {
                errors.push(format!("{}.{}: {err}", file.path.display(), file.format));
                continue;
            }
        };
        // The backup's name is known only in phase 2 (`free_backup`); it is added there.
        let note = format!(
            "  migrated {} ({}): v{} -> v{}",
            file.path.display(),
            file.format,
            file.found,
            file.current,
        );
        match at.and_then(|i| staged_files.get_mut(i)) {
            Some(staged) => {
                staged.output = output;
                staged.notes.push(note);
            }
            None => staged_files.push(StagedFile {
                rel: file.path.clone(),
                original: input,
                output,
                notes: vec![note],
            }),
        }
    }

    let mut staged_dirs: Vec<StagedDir> = Vec::new();
    let stage_dir = |rel: &Path, staged_dirs: &mut Vec<StagedDir>| -> std::io::Result<usize> {
        if let Some(i) = staged_dirs.iter().position(|d| d.rel == rel) {
            return Ok(i);
        }
        let root = args.project.join(rel);
        let name = root
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let stage = root.with_file_name(format!(".{name}.{}.stage", std::process::id()));
        if stage.symlink_metadata().is_ok() {
            std::fs::remove_dir_all(&stage)?;
        }
        staged_dirs.push(StagedDir {
            rel: rel.to_path_buf(),
            stage: stage.clone(),
            notes: Vec::new(),
        });
        copy_dir(&root, &stage.join(&name))?;
        Ok(staged_dirs.len() - 1)
    };
    for dir in level_dirs.iter().filter(|d| d.needs_migration()) {
        let staged = match stage_dir(&dir.path, &mut staged_dirs) {
            Ok(i) => &mut staged_dirs[i],
            Err(err) => {
                errors.push(format!("{} (level-dir): {err}", dir.path.display()));
                continue;
            }
        };
        match regia_scene::level_dir::upgrade_level_dir(&staged.copy()) {
            Ok(_) => staged.notes.push(format!(
                "  migrated {} (level-dir): {}",
                dir.path.display(),
                dir.status(),
            )),
            Err(err) => errors.push(format!("{} (level-dir): {err}", dir.path.display())),
        }
    }
    // Components inside level directories, on the copy the step above migrated (so they see every
    // directory at the current scene version).
    for file in files
        .iter()
        .filter(|f| f.needs_migration() && args.project.join(&f.path).is_dir())
    {
        let Some(component) = super::upgrade_components::embedded_format(file.format) else {
            continue;
        };
        let staged = match stage_dir(&file.path, &mut staged_dirs) {
            Ok(i) => &mut staged_dirs[i],
            Err(err) => {
                errors.push(format!("{}.{}: {err}", file.path.display(), file.format));
                continue;
            }
        };
        match super::upgrade_components::migrate_level_dir_components(&component, &staged.copy()) {
            Ok(written) => staged.notes.push(format!(
                "  migrated {} ({}): v{} -> v{}, {written} file(s) rewritten",
                file.path.display(),
                file.format,
                file.found,
                file.current,
            )),
            Err(err) => errors.push(format!("{}.{}: {err}", file.path.display(), file.format)),
        }
    }

    // Removes everything this run wrote before the first rename: staged files, staged
    // directories and new backups.
    let discard = |staged_files: &[StagedFile], staged_dirs: &[StagedDir], backups: &[PathBuf]| {
        for file in staged_files {
            let staged = staged_path(&args.project.join(&file.rel));
            if staged.symlink_metadata().is_ok() {
                let _ = std::fs::remove_file(&staged);
            }
        }
        for dir in staged_dirs {
            let _ = std::fs::remove_dir_all(&dir.stage);
        }
        for backup in backups {
            let _ = std::fs::remove_file(backup);
        }
    };
    if !errors.is_empty() {
        discard(&staged_files, &staged_dirs, &[]);
        anyhow::bail!(
            "upgrade-project refused (nothing written): {} migration error(s): {}",
            errors.len(),
            errors.join("; ")
        );
    }

    // Phase 2: stage every file, then write every backup, then rename.
    for file in &staged_files {
        if let Err(err) = write_staged(&staged_path(&args.project.join(&file.rel)), &file.output) {
            discard(&staged_files, &staged_dirs, &[]);
            anyhow::bail!(
                "upgrade-project refused (nothing written): {}: staging failed: {err}",
                file.rel.display()
            );
        }
    }
    let mut backups: Vec<PathBuf> = Vec::with_capacity(staged_files.len());
    for file in &staged_files {
        let backup = free_backup(args.project.join(format!("{}.bak", file.rel.display())));
        if let Err(err) = write_atomic(&backup, &file.original) {
            discard(&staged_files, &staged_dirs, &backups);
            anyhow::bail!(
                "upgrade-project refused (nothing written): {}: backup failed: {err}",
                file.rel.display()
            );
        }
        backups.push(backup);
    }
    let mut failed: Vec<String> = Vec::new();
    let (mut migrated, mut backed_up) = (0usize, backups.len());
    for (file, backup) in staged_files.iter().zip(&backups) {
        let path = args.project.join(&file.rel);
        match std::fs::rename(staged_path(&path), &path) {
            Ok(()) => {
                migrated += file.notes.len();
                print_notes(&file.notes, &backup_label(&args.project, backup));
            }
            Err(err) => {
                let _ = std::fs::remove_file(staged_path(&path));
                failed.push(format!("{}: {err}", file.rel.display()));
            }
        }
    }
    for dir in &staged_dirs {
        let root = args.project.join(&dir.rel);
        let backup = free_backup(args.project.join(format!("{}.bak", dir.rel.display())));
        let swapped = std::fs::rename(&root, &backup).and_then(|()| {
            std::fs::rename(dir.copy(), &root).inspect_err(|_| {
                // Put the original back where it was.
                let _ = std::fs::rename(&backup, &root);
            })
        });
        let _ = std::fs::remove_dir_all(&dir.stage);
        match swapped {
            Ok(()) => {
                backed_up += 1;
                migrated += dir.notes.len();
                print_notes(&dir.notes, &backup_label(&args.project, &backup));
            }
            Err(err) => failed.push(format!("{} (level-dir): {err}", dir.rel.display())),
        }
    }
    if !failed.is_empty() {
        anyhow::bail!(
            "upgrade-project: {} file(s) not renamed into place (their backups are kept): {}",
            failed.len(),
            failed.join("; ")
        );
    }
    Ok((migrated, backed_up))
}

/// Prints one file's or directory's migration notes; the first names the one backup it got (the
/// name `free_backup` picked: `.bak.2` when an earlier run left a `.bak`), so a file migrated
/// twice in one run (the scene, then its components) names its backup once.
fn print_notes(notes: &[String], backup: &str) {
    for (i, note) in notes.iter().enumerate() {
        if i == 0 {
            eprintln!("{note} (backup: {backup})");
        } else {
            eprintln!("{note}");
        }
    }
}

/// Recursive copy (the backup of a level directory).
fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// `<file>.tmp` next to `path` (same directory, so a rename stays on one filesystem).
fn staged_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    // Named by process: two runs must not write into one staging file.
    name.push(format!(".{}.tmp", std::process::id()));
    path.with_file_name(name)
}

/// `path`, or `path.2`, `path.3`, ... when an earlier run already left that backup: the first
/// `.bak` is the only copy of the file from before any migration.
fn free_backup(path: PathBuf) -> PathBuf {
    if !path.exists() {
        return path;
    }
    let mut n = 2u32;
    loop {
        let mut name = path.file_name().unwrap_or_default().to_os_string();
        name.push(format!(".{n}"));
        let next = path.with_file_name(name);
        if !next.exists() {
            return next;
        }
        n += 1;
    }
}

/// Writes `bytes` to a staged path as a new file: a leftover (or a planted symlink) at that name is
/// removed first, never written through (ADR-1816: a project could ship `x.ron.tmp` as a link).
fn write_staged(staged: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    if staged.symlink_metadata().is_ok() {
        std::fs::remove_file(staged)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(staged)?;
    file.write_all(bytes)
}

/// Writes `bytes` to `path` through a temp file and a rename.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let staged = staged_path(path);
    write_staged(&staged, bytes)?;
    std::fs::rename(&staged, path)
}

/// The game-executable crates of a template project, relative to the project: the workspace
/// templates' `launcher`, the net game's `client` and dedicated `server` (ADR-5617), and the
/// quick template's root crate.
const GAME_EXECUTABLE_DIRS: [&str; 4] = ["launcher", "client", "server", ""];

/// What `--write` does about one game executable's Windows build script (ADR-2645).
#[derive(Debug, Clone, PartialEq, Eq)]
enum BuildScriptAction {
    /// No build script: write `build.rs` and add the build-dependencies to `Cargo.toml`.
    Add,
    /// The build script is the template's but `Cargo.toml` lacks some build-dependencies.
    Dependencies,
    /// The build script is an unchanged copy of an earlier release's template script (ADR-5181):
    /// replace it with the current one and add any missing build-dependencies.
    Update,
    /// The build script and its dependencies are in place.
    Current,
    /// The crate has a build script of its own (edited, or another `build =` key): left alone.
    UserScript,
    /// The build-dependencies could not be added by a text edit (no `[build-dependencies]`
    /// header line to extend): the user adds them.
    Manual,
}

/// One game executable crate and its [`BuildScriptAction`].
#[derive(Debug, Clone, PartialEq, Eq)]
struct BuildScriptPlan {
    /// The crate directory relative to the project (`""` for the root crate).
    dir: &'static str,
    /// What `--write` does.
    action: BuildScriptAction,
}

impl BuildScriptPlan {
    /// `launcher/build.rs`, or `build.rs` for the root crate.
    fn script(&self) -> String {
        if self.dir.is_empty() {
            "build.rs".to_owned()
        } else {
            format!("{}/build.rs", self.dir)
        }
    }

    /// The crate's manifest path relative to the project.
    fn manifest(&self) -> String {
        if self.dir.is_empty() {
            "Cargo.toml".to_owned()
        } else {
            format!("{}/Cargo.toml", self.dir)
        }
    }

    /// The `--json` status word.
    fn status(&self) -> &'static str {
        match self.action {
            BuildScriptAction::Add => "add",
            BuildScriptAction::Dependencies => "dependencies",
            BuildScriptAction::Update => "update",
            BuildScriptAction::Current => "current",
            BuildScriptAction::UserScript => "user_script",
            BuildScriptAction::Manual => "manual",
        }
    }
}

/// `text` with every build-dependency of the template's Windows build script present: the
/// template's table appended when the manifest has none, else the missing lines inserted under
/// its `[build-dependencies]` header. `None` when the table exists without a header line to
/// extend (dotted or inline keys), or the edit would not parse.
fn with_build_dependencies(text: &str) -> Option<String> {
    let table: toml::Table = text.parse().ok()?;
    let existing = table.get("build-dependencies").and_then(toml::Value::as_table);
    let missing: Vec<String> = regia_templates::WINDOWS_BUILD_DEPENDENCIES
        .iter()
        .filter(|(name, _)| existing.is_none_or(|t| !t.contains_key(*name)))
        .map(|(name, value)| format!("{name} = {value}"))
        .collect();
    if missing.is_empty() {
        return Some(text.to_owned());
    }
    let edited = if existing.is_none() {
        let mut out = text.to_owned();
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
        out.push_str(&regia_templates::windows_build_dependencies_table());
        out
    } else {
        let mut out = String::with_capacity(text.len() + 160);
        let mut inserted = false;
        for line in text.split_inclusive('\n') {
            out.push_str(line);
            if !inserted && line.trim() == "[build-dependencies]" {
                if !line.ends_with('\n') {
                    out.push('\n');
                }
                for dep in &missing {
                    out.push_str(dep);
                    out.push('\n');
                }
                inserted = true;
            }
        }
        if !inserted {
            return None;
        }
        out
    };
    let check: toml::Table = edited.parse().ok()?;
    let deps = check.get("build-dependencies").and_then(toml::Value::as_table)?;
    regia_templates::WINDOWS_BUILD_DEPENDENCIES
        .iter()
        .all(|(name, _)| deps.contains_key(*name))
        .then_some(edited)
}

/// The Windows build script plan of every game executable crate of `project`: a crate with a
/// `[package]`, a `src/main.rs` and a `regia` dependency. A project without such crates (or with
/// manifests that do not parse) plans nothing.
fn plan_build_scripts(project: &Path) -> Vec<BuildScriptPlan> {
    let mut plans = Vec::new();
    for dir in GAME_EXECUTABLE_DIRS {
        let root = project.join(dir);
        let Ok(text) = std::fs::read_to_string(root.join("Cargo.toml")) else {
            continue;
        };
        let Ok(manifest) = text.parse::<toml::Table>() else {
            continue;
        };
        let Some(package) = manifest.get("package").and_then(toml::Value::as_table) else {
            continue;
        };
        let links_regia = manifest
            .get("dependencies")
            .and_then(toml::Value::as_table)
            .is_some_and(|deps| deps.contains_key("regia"));
        if !links_regia || !root.join("src/main.rs").is_file() {
            continue;
        }
        let script = root.join("build.rs");
        let custom_build = package
            .get("build")
            .is_some_and(|b| b.as_str() != Some("build.rs"));
        let action = if custom_build {
            BuildScriptAction::UserScript
        } else if script.exists() {
            let existing = std::fs::read_to_string(&script).unwrap_or_default();
            let ours = existing.replace("\r\n", "\n") == regia_templates::WINDOWS_BUILD_SCRIPT;
            let earlier = regia_templates::is_earlier_windows_build_script(&existing);
            match (ours, with_build_dependencies(&text)) {
                (false, Some(_)) if earlier => BuildScriptAction::Update,
                // An earlier engine script is still the engine's: only the manifest needs a hand.
                (false, None) if earlier => BuildScriptAction::Manual,
                (false, _) => BuildScriptAction::UserScript,
                (true, Some(edited)) if edited == text => BuildScriptAction::Current,
                (true, Some(_)) => BuildScriptAction::Dependencies,
                (true, None) => BuildScriptAction::Manual,
            }
        } else {
            match with_build_dependencies(&text) {
                Some(_) => BuildScriptAction::Add,
                None => BuildScriptAction::Manual,
            }
        };
        plans.push(BuildScriptPlan { dir, action });
    }
    plans
}

/// Prints the dry-run line of each build script plan that changes something.
fn print_build_script_plans(plans: &[BuildScriptPlan]) {
    for plan in plans {
        let line = match plan.action {
            BuildScriptAction::Add => format!(
                "{} would be added and {} would get its [build-dependencies] (the Windows exe icon and version info, ADR-2645)",
                plan.script(),
                plan.manifest()
            ),
            BuildScriptAction::Update => format!(
                "{} is an earlier engine's script and would be replaced by the current one (the exe's main-thread stack, ADR-5181)",
                plan.script()
            ),
            BuildScriptAction::Dependencies => {
                format!(
                    "{} would get the build-dependencies of {}",
                    plan.manifest(),
                    plan.script()
                )
            }
            BuildScriptAction::UserScript => format!(
                "{}: your own build script, left alone (copy the Windows resources from a new project's launcher/build.rs if you want them)",
                plan.script()
            ),
            BuildScriptAction::Manual => format!(
                "{}: add {} under [build-dependencies] by hand for the Windows build script",
                plan.manifest(),
                regia_templates::WINDOWS_BUILD_DEPENDENCIES
                    .iter()
                    .map(|(name, value)| format!("`{name} = {value}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            BuildScriptAction::Current => continue,
        };
        eprintln!("  {line}");
    }
}

/// Why `--write` wrote a build script whose manifest needed no edit: an [`BuildScriptAction::Add`]
/// restores the icon and version info (ADR-2645); an [`BuildScriptAction::Update`] brings an
/// earlier engine script up to date (the main-thread stack, ADR-5181).
fn added_script_reason(action: &BuildScriptAction) -> &'static str {
    match action {
        BuildScriptAction::Update => "the exe's main-thread stack, ADR-5181",
        _ => "the Windows exe icon and version info, ADR-2645",
    }
}

/// Applies each build script plan: writes the template's `build.rs` where there is none and
/// rewrites the crate's `Cargo.toml` (with a `.bak`) to add its build-dependencies. The manifest
/// is read again here, so the engine dependency edits written before it are kept. Returns the
/// number of crates changed.
fn apply_build_script_plans(project: &Path, plans: &[BuildScriptPlan]) -> std::io::Result<usize> {
    let mut changed = 0;
    for plan in plans {
        let (add, verb) = match &plan.action {
            BuildScriptAction::Add => (true, "added"),
            BuildScriptAction::Update => (true, "updated"),
            BuildScriptAction::Dependencies => (false, ""),
            BuildScriptAction::Current | BuildScriptAction::UserScript | BuildScriptAction::Manual => {
                continue;
            }
        };
        let current = std::fs::read_to_string(project.join(plan.manifest()))?;
        let Some(manifest) = with_build_dependencies(&current) else {
            eprintln!(
                "  {}: the build-dependencies could not be added; {} not written",
                plan.manifest(),
                plan.script()
            );
            continue;
        };
        if add {
            write_atomic(
                &project.join(plan.script()),
                regia_templates::WINDOWS_BUILD_SCRIPT.as_bytes(),
            )?;
        }
        let backup = if manifest != current {
            replace_with_backup(project, &plan.manifest(), &manifest)?
        } else {
            String::new()
        };
        if add && manifest == current {
            eprintln!(
                "  {verb} {} ({})",
                plan.script(),
                added_script_reason(&plan.action)
            );
            changed += 1;
            continue;
        }
        eprintln!(
            "  {}{}: [build-dependencies] for the Windows icon and version info (backup: {backup})",
            if add {
                format!("{verb} {}, ", plan.script())
            } else {
                String::new()
            },
            plan.manifest(),
        );
        changed += 1;
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-7750 (the ADR-7724 (h) fold): a v3 bake upgrades to the early v4 layout (no override
    /// section), and a v4 bake with overrides is already current: the row leaves it byte for byte,
    /// so a second `upgrade-project` changes nothing.
    #[test]
    fn pgout_upgrades_keep_the_override_fold_idempotent() {
        let corpus = |name: &str| {
            std::fs::read(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../regia_agent/tests/fixtures/format-corpus/pgout")
                    .join(name),
            )
            .expect("a pgout corpus fixture")
        };
        let v3 = corpus("v3.bin");
        let up = migrate_pgout(&v3, 3).expect("v3 upgrades");
        assert_eq!(&up[4..8], &4u32.to_le_bytes(), "now version 4");
        let (_, output) = regia_procgen::pgout::decode(&up).expect("loads");
        assert!(output.prefabs.iter().all(|p| p.overrides.is_empty()));
        assert_eq!(migrate_pgout(&up, 4).expect("again"), up, "idempotent");
        let with = corpus("v4-overrides.bin");
        let (_, output) = regia_procgen::pgout::decode(&with).expect("loads");
        assert!(
            output.prefabs.iter().any(|p| !p.overrides.is_empty()),
            "the fixture carries overrides"
        );
        assert_eq!(migrate_pgout(&with, 4).expect("current"), with, "unchanged");
    }

    /// ADR-1816: a symlink planted at the staged name is replaced, not written through.
    #[cfg(unix)]
    #[test]
    fn staged_writes_never_follow_a_planted_symlink() {
        let dir = regia_test_utils::unique_temp_dir("upgrade-staged-link");
        let victim = dir.join("victim.txt");
        std::fs::write(&victim, "keep").expect("victim");
        let target = dir.join("level.ron");
        std::os::unix::fs::symlink(&victim, staged_path(&target)).expect("link");
        write_atomic(&target, b"new").expect("write");
        assert_eq!(std::fs::read_to_string(&victim).expect("read"), "keep");
        assert_eq!(std::fs::read_to_string(&target).expect("read"), "new");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_v1_fracture_set_gains_its_render_meshes_with_a_backup() {
        use regia_asset_pipeline::fracture::{FRACTURE_FORMAT_VERSION, FractureSet};
        let project = regia_test_utils::unique_temp_dir("upgrade_fracture");
        let assets = project.join("assets");
        std::fs::create_dir_all(&assets).expect("assets");
        let file = assets.join("tetra.fracture.ron");
        let v1 = "(format_version: 1, source_volume: 0.16666667, chunks: [(site: (0.25, 0.25, 0.25), \
                  positions: [(0.0, 0.0, 0.0), (1.0, 0.0, 0.0), (0.0, 1.0, 0.0), (0.0, 0.0, 1.0)], \
                  indices: [0, 2, 1, 0, 1, 3, 0, 3, 2, 1, 2, 3], interior_start: 3, volume: 0.16666667, \
                  centroid: (0.25, 0.25, 0.25), bounds_min: (0.0, 0.0, 0.0), bounds_max: (1.0, 1.0, 1.0), \
                  interior_area: 0.0)], joints: [])";
        std::fs::write(&file, v1).expect("write");
        run_with(
            &UpgradeProjectArgs {
                project: project.clone(),
                write: true,
                engine: None,
                json: false,
                backup: None,
            },
            &formats(),
        )
        .expect("upgrade");
        let up = FractureSet::from_ron(&std::fs::read_to_string(&file).expect("read")).expect("loads");
        assert_eq!(up.format_version, FRACTURE_FORMAT_VERSION);
        assert_eq!(up.chunks[0].render.indices.len(), 12);
        assert_eq!(up.chunks[0].render.interior_start, 3);
        let backup = assets.join("tetra.fracture.ron.bak");
        assert_eq!(std::fs::read_to_string(backup).expect("backup"), v1);
        let _ = std::fs::remove_dir_all(&project);
    }

    #[test]
    fn a_v1_vehicle_becomes_v2_with_the_simple_drivetrain_and_a_backup() {
        use regia::physics::vehicle_asset::{VEHICLE_FORMAT_VERSION, VehicleAsset};
        use regia::physics::vehicle_drivetrain::Drivetrain;
        let project = regia_test_utils::unique_temp_dir("upgrade_vehicle");
        let assets = project.join("assets");
        std::fs::create_dir_all(&assets).expect("assets");
        let file = assets.join("car.vehicle.ron");
        let v1 = "// tuned\n(format_version: 1, name: \"car\", config: (max_engine_force: 4200.5))";
        std::fs::write(&file, v1).expect("write");
        run_with(
            &UpgradeProjectArgs {
                project: project.clone(),
                write: true,
                engine: None,
                json: false,
                backup: None,
            },
            &formats(),
        )
        .expect("upgrade");
        let text = std::fs::read_to_string(&file).expect("read");
        assert_eq!(
            regia_core::version::peek_ron_version(&text).expect("version"),
            VEHICLE_FORMAT_VERSION
        );
        let up = VehicleAsset::from_ron(&text).expect("loads");
        assert_eq!(up.config.drivetrain, Drivetrain::Simple);
        assert_eq!(up.config.max_engine_force.to_bits(), 4200.5_f32.to_bits());
        let backup = assets.join("car.vehicle.ron.bak");
        assert_eq!(std::fs::read_to_string(backup).expect("backup"), v1);
        let _ = std::fs::remove_dir_all(&project);
    }

    #[test]
    fn a_v2_vehicle_becomes_v3_with_damper_tires_and_a_backup() {
        use regia::physics::vehicle_asset::{VEHICLE_FORMAT_VERSION, VehicleAsset};
        use regia::physics::vehicle_tire::TireModel;
        let project = regia_test_utils::unique_temp_dir("upgrade_vehicle_v2");
        let assets = project.join("assets");
        std::fs::create_dir_all(&assets).expect("assets");
        let file = assets.join("geared.vehicle.ron");
        let v2 = "(format_version: 2, name: \"geared\", config: (drivetrain: Geared(()), max_steer_angle: 0.4125))";
        std::fs::write(&file, v2).expect("write");
        run_with(
            &UpgradeProjectArgs {
                project: project.clone(),
                write: true,
                engine: None,
                json: false,
                backup: None,
            },
            &formats(),
        )
        .expect("upgrade");
        let text = std::fs::read_to_string(&file).expect("read");
        assert_eq!(VEHICLE_FORMAT_VERSION, 4);
        assert_eq!(
            regia_core::version::peek_ron_version(&text).expect("version"),
            VEHICLE_FORMAT_VERSION
        );
        let up = VehicleAsset::from_ron(&text).expect("loads");
        assert!(up.config.wheels.iter().all(|w| w.tire_model == TireModel::Damper));
        assert_eq!(up.config.max_steer_angle.to_bits(), 0.4125_f32.to_bits());
        assert_eq!(Ok(up), VehicleAsset::from_ron(v2));
        let backup = assets.join("geared.vehicle.ron.bak");
        assert_eq!(std::fs::read_to_string(backup).expect("backup"), v2);
        let _ = std::fs::remove_dir_all(&project);
    }

    #[test]
    fn a_mixed_future_level_directory_refuses_before_writing_other_files() {
        let project = regia_test_utils::unique_temp_dir("upgrade_future_level_dir");
        let root = project.join("assets/levels/yard.level.d");
        let old_guid = regia_core::id::Guid::from_u128(0xab);
        let new_guid = regia_core::id::Guid::from_u128(0xcd);
        let mut scene = regia_scene::format::SceneFile::new();
        for guid in [old_guid, new_guid] {
            scene.entities.push(regia_scene::format::SceneEntity {
                guid,
                ..Default::default()
            });
        }
        regia_scene::level_dir::write_level_dir(&root, &scene).expect("write dir");
        let current = regia_scene::format::SCENE_FORMAT_VERSION;
        let mut originals = Vec::new();
        for (guid, version) in [(old_guid, current - 1), (new_guid, current + 1)] {
            let path = root.join(regia_scene::level_dir::entity_rel_path(guid));
            let text = std::fs::read_to_string(&path).expect("entity");
            let text = text.replace(
                &format!("scene_format_version: {current}"),
                &format!("scene_format_version: {version}"),
            );
            std::fs::write(&path, &text).expect("set version");
            originals.push((path, text));
        }
        let cue = project.join("sound.cue.ron");
        let original_cue = "( name: \"kept\", sounds: [] )";
        std::fs::write(&cue, original_cue).expect("cue");
        let result = run_with(
            &UpgradeProjectArgs {
                project: project.clone(),
                write: true,
                engine: None,
                json: false,
                backup: None,
            },
            &formats(),
        );
        let error = result.expect_err("future directory must not look like a clean migration");
        assert!(error.to_string().contains("yard.level.d"), "{error}");
        for (path, text) in originals {
            assert_eq!(std::fs::read_to_string(path).expect("unchanged entity"), text);
        }
        assert_eq!(std::fs::read_to_string(cue).expect("unchanged cue"), original_cue);
        assert!(!project.join("sound.cue.ron.bak").exists());
        assert!(!project.join("assets/levels/yard.level.d.bak").exists());
        std::fs::remove_dir_all(project).expect("cleanup");
    }

    #[test]
    fn level_dirs_with_old_files_are_upgraded_in_place_with_a_backup() {
        let project = regia_test_utils::unique_temp_dir("upgrade_level_dir");
        let root = project.join("assets/levels/yard.level.d");
        let mut scene = regia_scene::format::SceneFile::new();
        scene.entities.push(regia_scene::format::SceneEntity {
            guid: regia_core::id::Guid::from_u128(0xab),
            ..Default::default()
        });
        regia_scene::level_dir::write_level_dir(&root, &scene).expect("write dir");
        let entity = root.join(regia_scene::level_dir::entity_rel_path(
            regia_core::id::Guid::from_u128(0xab),
        ));
        let current = regia_scene::format::SCENE_FORMAT_VERSION;
        let text = std::fs::read_to_string(&entity).expect("read");
        std::fs::write(
            &entity,
            text.replace(
                &format!("scene_format_version: {current}"),
                &format!("scene_format_version: {}", current - 1),
            ),
        )
        .expect("age one file");
        let plans = plan_level_dirs(&project).expect("level directories");
        assert_eq!(plans.len(), 1);
        assert!(plans[0].needs_migration(), "{}", plans[0].status());

        run_with(
            &UpgradeProjectArgs {
                project: project.clone(),
                write: true,
                engine: None,
                json: false,
                backup: None,
            },
            &[],
        )
        .expect("upgrade");
        assert!(project.join("assets/levels/yard.level.d.bak/level.ron").is_file());
        assert_eq!(
            std::fs::read_to_string(&entity).expect("read"),
            text,
            "rewritten at the latest version"
        );
        assert!(!plan_level_dirs(&project).expect("level directories")[0].needs_migration());
        regia_scene::level_dir::read_level_dir(&root).expect("readable");
    }

    /// A level directory with an old post volume and lens in one entity file, that file also one
    /// scene version behind: the plan lists both components under the directory, `--write`
    /// upgrades the directory and then its components, keeps one `.bak` with the original files,
    /// leaves the other entity file alone, and the values survive (spec 83 § 3.3).
    #[test]
    fn level_dir_components_are_listed_and_upgraded_after_the_directory() {
        use super::super::upgrade_components::{PHYSICAL_CAMERA_TYPE_PATH, POST_VOLUME_TYPE_PATH};
        let project = regia_test_utils::unique_temp_dir("upgrade_level_dir_components");
        let root = project.join("assets/levels/hall.level.d");
        let volume_guid = regia_core::id::Guid::from_u128(0xab);
        let plain_guid = regia_core::id::Guid::from_u128(0xcd);
        let raw = |text: &str| {
            ron::value::RawValue::from_boxed_ron(text.to_string().into_boxed_str()).expect("raw ron")
        };
        let mut scene = regia_scene::format::SceneFile::new();
        let mut volume = regia_scene::format::SceneEntity {
            guid: volume_guid,
            ..Default::default()
        };
        volume.components.insert(
            POST_VOLUME_TYPE_PATH.to_string(),
            raw("(format_version: 1, shape: Box(half_extents: (2.0, 3.0, 4.0)), priority: 2)"),
        );
        volume.components.insert(
            PHYSICAL_CAMERA_TYPE_PATH.to_string(),
            raw("(shutter_angle: 0.25)"),
        );
        scene.entities.push(volume);
        scene.entities.push(regia_scene::format::SceneEntity {
            guid: plain_guid,
            ..Default::default()
        });
        regia_scene::level_dir::write_level_dir(&root, &scene).expect("write dir");
        let entity = root.join(regia_scene::level_dir::entity_rel_path(volume_guid));
        let plain = root.join(regia_scene::level_dir::entity_rel_path(plain_guid));
        let current = regia_scene::format::SCENE_FORMAT_VERSION;
        let text = std::fs::read_to_string(&entity).expect("read");
        let aged = text.replace(
            &format!("scene_format_version: {current}"),
            &format!("scene_format_version: {}", current - 1),
        );
        std::fs::write(&entity, &aged).expect("age the file");
        let plain_before = std::fs::read(&plain).expect("read");

        let dirs = plan_level_dirs(&project).expect("level directories");
        let mut files = Vec::new();
        plan_embedded_dirs(&project, &dirs, &mut files);
        let listed: Vec<(&str, u32)> = files.iter().map(|f| (f.format, f.found)).collect();
        assert_eq!(listed, vec![("post-volume", 1), ("physical-camera", 0)]);
        assert!(
            files
                .iter()
                .all(|f| f.path == Path::new("assets/levels/hall.level.d"))
        );

        run_with(
            &UpgradeProjectArgs {
                project: project.clone(),
                write: true,
                engine: None,
                json: false,
                backup: None,
            },
            &[],
        )
        .expect("upgrade");
        let mut after = Vec::new();
        plan_embedded_dirs(
            &project,
            &plan_level_dirs(&project).expect("level directories"),
            &mut after,
        );
        assert_eq!(after.len(), 2);
        assert!(after.iter().all(|f| !f.needs_migration()), "components current");
        let backup = project.join("assets/levels/hall.level.d.bak");
        assert!(backup.join("level.ron").is_file());
        assert!(
            !project.join("assets/levels/hall.level.d.bak.2").exists(),
            "one backup"
        );
        assert_eq!(
            std::fs::read_to_string(backup.join(regia_scene::level_dir::entity_rel_path(volume_guid)))
                .expect("backup entity"),
            aged,
            "the backup holds the original bytes"
        );
        assert_eq!(std::fs::read(&plain).expect("read"), plain_before, "untouched");

        let (scene, _) = regia_scene::level_dir::read_level_dir(&root).expect("readable");
        let stored = scene
            .entities
            .iter()
            .find(|e| e.guid == volume_guid)
            .expect("entity kept");
        let mut registry = regia_ecs::reflect::TypeRegistry::default();
        regia_render::post_volumes::register_post_volume_types(&mut registry);
        regia_render::register_physical_camera_types(&mut registry);
        let decode = |path: &str| {
            let registration = registry.get_with_type_path(path).expect("registered");
            regia_ecs::serialize::ReflectCodec::new(&registry)
                .from_ron_typed(stored.components[path].get_ron(), registration)
                .expect("decodes")
        };
        let volume = decode(POST_VOLUME_TYPE_PATH)
            .downcast::<regia_render::PostVolume>()
            .expect("a post volume");
        assert_eq!(
            volume.format_version,
            regia_render::post_volumes::POST_VOLUME_FORMAT_VERSION
        );
        assert_eq!(
            volume.shape,
            regia_render::PostVolumeShape::Box {
                half_extents: [2.0, 3.0, 4.0]
            }
        );
        assert_eq!(volume.priority, 2);
        let lens = decode(PHYSICAL_CAMERA_TYPE_PATH)
            .downcast::<regia_render::PhysicalCamera>()
            .expect("a lens");
        assert_eq!(lens.shutter_angle, 0.25);
        assert_eq!(lens.format_version, regia_render::PHYSICAL_CAMERA_FORMAT_VERSION);
        let _ = std::fs::remove_dir_all(&project);
    }

    #[test]
    fn brand_moves_rename_old_names_once_and_never_overwrite() {
        let dir = regia_test_utils::unique_temp_dir("upgrade_brand");
        let names = [("Ferrite.toml", "Regia.toml"), (".ferrite", ".regia")]; // rename-keep
        std::fs::write(dir.join(names[0].0), "[project]\n").expect("manifest");
        std::fs::create_dir_all(dir.join(names[1].0).join("cooked")).expect("state dir");
        let moves = brand_moves(&dir, &names);
        assert_eq!(moves.len(), 2, "{moves:?}");
        apply_brand_moves(&moves).expect("rename");
        assert!(dir.join(names[0].1).is_file());
        assert!(
            dir.join(names[1].1).join("cooked").is_dir(),
            "the directory moves with its contents"
        );
        assert!(brand_moves(&dir, &names).is_empty(), "nothing left to move");
        // An old name next to an existing current one is left alone.
        std::fs::write(dir.join(names[0].0), "old").expect("old manifest again");
        assert!(brand_moves(&dir, &names).is_empty());
        // Names that agree (every pair before the rename) never move.
        assert!(brand_moves(&dir, &[(".regia", ".regia")]).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_moved_state_directory_stays_ignored() {
        let legacy = regia_core::brand::LEGACY_PROJECT_STATE_DIR;
        let current = regia_core::brand::PROJECT_STATE_DIR;
        // The `.gitignore` a project created before the rename has (its template's lines).
        let old = format!("/target\n/{legacy}\n*.bak\n");
        if legacy == current {
            assert_eq!(gitignore_line(&old, legacy, current), None);
            return;
        }
        assert_eq!(gitignore_line(&old, legacy, current), Some(format!("/{current}")));
        assert_eq!(
            gitignore_line(&format!("{legacy}/\n"), legacy, current),
            Some(format!("{current}/"))
        );
        assert_eq!(
            gitignore_line(&format!("{old}/{current}\n"), legacy, current),
            None,
            "already ignored"
        );
        assert_eq!(
            gitignore_line("/target\n", legacy, current),
            None,
            "never ignored"
        );

        let dir = regia_test_utils::unique_temp_dir("upgrade_gitignore");
        std::fs::write(dir.join(".gitignore"), &old).expect("gitignore");
        std::fs::create_dir_all(dir.join(legacy)).expect("state dir");
        let moves = brand_moves(&dir, &brand_names());
        assert_eq!(gitignore_addition(&dir, &moves), Some(format!("/{current}")));
        assert_eq!(gitignore_addition(&dir, &[]), None, "no move, no line");
        // Only the manifest moves (no state directory yet): the new one appears later, ignored.
        std::fs::remove_dir_all(dir.join(legacy)).expect("drop state dir");
        std::fs::write(dir.join(regia_core::brand::LEGACY_MANIFEST_FILE), "[project]\n").expect("manifest");
        let moves = brand_moves(&dir, &brand_names());
        assert_eq!(moves.len(), 1, "{moves:?}");
        assert_eq!(gitignore_addition(&dir, &moves), Some(format!("/{current}")));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// w10-corr-low-landing (planner item 3b): a replace that cannot read its original is an
    /// error the caller propagates with `?`, never a silent no-op, and it leaves no backup behind.
    #[test]
    fn a_failed_replace_is_an_error_not_silent() {
        let dir = regia_test_utils::unique_temp_dir("upgrade_backup_fail");
        std::fs::create_dir_all(dir.join("dir.ron")).expect("test setup");
        let missing = replace_with_backup(&dir, "missing.ron", "v2");
        assert!(missing.is_err(), "{missing:?}");
        let not_a_file = replace_with_backup(&dir, "dir.ron", "v2");
        assert!(not_a_file.is_err(), "{not_a_file:?}");
        assert!(
            !dir.join("missing.ron").exists(),
            "nothing written over a missing original"
        );
        assert!(!dir.join("missing.ron.bak").exists());
        assert!(!dir.join("dir.ron.bak").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_second_run_keeps_the_first_backup() {
        let dir = regia_test_utils::unique_temp_dir("upgrade_backups");
        std::fs::create_dir_all(&dir).expect("test setup");
        std::fs::write(dir.join("a.ron"), "v1").expect("test setup");
        let first = replace_with_backup(&dir, "a.ron", "v2").expect("first");
        let second = replace_with_backup(&dir, "a.ron", "v3").expect("second");
        let third = replace_with_backup(&dir, "a.ron", "v4").expect("third");
        // The messages name the backup the run actually wrote, not always `.bak` (corr-low cli:
        // a rerun lands on `.bak.2` while the note said `.bak`).
        assert_eq!(first, "a.ron.bak");
        assert_eq!(second, "a.ron.bak.2");
        assert_eq!(third, "a.ron.bak.3");
        assert_eq!(
            std::fs::read_to_string(dir.join("a.ron.bak")).expect("first backup"),
            "v1"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("a.ron.bak.2")).expect("second"),
            "v2"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("a.ron.bak.3")).expect("third"),
            "v3"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_tag_of_another_engine_is_refused_and_a_tag_with_no_version_only_warns() {
        let source = |tag: &str| EngineSource::Git {
            url: "https://example.com/regia.git".into(),
            tag: tag.into(),
        };
        let severity = |tag: &str| {
            let mut problems = Vec::new();
            check_source(Path::new("."), &source(tag), &mut problems);
            problems
                .iter()
                .map(|p| (p.code, p.severity == Severity::Error))
                .collect::<Vec<_>>()
        };
        assert_eq!(severity("v0.1.0"), [("engine_source_version_mismatch", true)]);
        assert_eq!(severity("stable"), [("engine_tag_unverified", false)]);
        assert!(severity(&format!("v{}", engine_version::CURRENT)).is_empty());
    }

    #[test]
    fn write_atomic_replaces_and_leaves_no_temp() {
        let dir = regia_test_utils::unique_temp_dir("upgrade_atomic");
        std::fs::create_dir_all(&dir).expect("test setup");
        let file = dir.join("a.level.ron.bak");
        write_atomic(&file, b"one").expect("test setup");
        write_atomic(&file, b"two").expect("test setup");
        assert_eq!(std::fs::read(&file).expect("test setup"), b"two");
        assert!(!staged_path(&file).exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reads_ron_version_first() {
        let bytes = b"( format_version: 1, name: \"x\", shader: Standard )";
        assert_eq!(read_ron_version(bytes).expect("version"), 1);

        let bare = b"( version: 2, root: () )";
        assert_eq!(read_ron_version(bare).expect("version"), 2);
    }

    #[test]
    fn reads_named_struct_scene_form() {
        // The published scene files use `SceneFile(...)`; the probe must still find the version.
        let bytes = b"SceneFile( format_version: 1, entities: [], resources: {} )";
        assert_eq!(read_ron_version(bytes).expect("version"), 1);
    }

    #[test]
    fn missing_version_is_an_error() {
        let bytes = b"( name: \"no version here\" )";
        assert!(read_ron_version(bytes).is_err());
    }

    #[test]
    fn reads_binary_header_versions() {
        let mut save = Vec::from(*b"FRTS");
        save.extend_from_slice(&1u32.to_le_bytes());
        assert_eq!(read_binary_version(&save, b"FRTS").expect("save"), 1);

        let mut pak = Vec::from(*b"FPAK");
        pak.extend_from_slice(&1u32.to_le_bytes());
        assert_eq!(read_binary_version(&pak, b"FPAK").expect("pak"), 1);

        assert!(read_binary_version(b"XXXX1234", b"FPAK").is_err(), "bad magic");
    }

    #[test]
    fn implicit_audio_versions_ignore_comments_and_accept_spaced_root_keys() {
        let old = b"( // version: 1 was the old default\n version: 2, name: \"intro\", sounds: [], )";
        let migrated = migrate_cue(old, 2).expect("cue migration");
        let text = std::str::from_utf8(&migrated).expect("utf8");
        assert!(text.contains("// version: 1 was the old default"));
        assert_eq!(read_implicit_v1(&migrated), Ok(regia::audio::CUE_VERSION));
        regia::audio::SoundCueDef::from_ron(text).expect("migrated cue loads");

        let migrated = migrate_music(b"( version : 1, name: \"explore\", bpm: 120.0, stems: [], )", 1)
            .expect("music migration");
        let text = std::str::from_utf8(&migrated).expect("utf8");
        assert_eq!(read_implicit_v1(&migrated), Ok(regia::audio::MUSIC_VERSION));
        regia::audio::MusicDef::from_ron(text).expect("one root version field");
    }

    #[test]
    fn music_version_key_comments_preserve_the_single_root_field() {
        let old = "(version /* retained */ : /* kept */ 1, name: \"legacy\", bpm: 120.0, stems: [])";
        regia::audio::MusicDef::from_ron(old).expect("valid legacy music");
        let migrated = migrate_music(old.as_bytes(), 1).expect("migration");
        let text = std::str::from_utf8(&migrated).expect("utf8");
        assert_eq!(
            text,
            old.replace(" 1,", &format!(" {},", regia::audio::MUSIC_VERSION))
        );
        regia::audio::MusicDef::from_ron(text).expect("one current root version");
    }

    #[test]
    fn malformed_implicit_audio_is_never_classified_or_migrated() {
        for bytes in [
            b"( name: \"truncated\", ".as_slice(),
            b"( version: \"bad\", )",
            b"[1, 2]",
        ] {
            assert!(read_implicit_v1(bytes).is_err());
            assert!(migrate_cue(bytes, 1).is_err());
            assert!(migrate_music(bytes, 1).is_err());
        }
    }

    #[test]
    fn legacy_data_tables_with_block_comments_and_attributes_are_migrated() {
        for prefix in ["/* exported /* nested */ */\n", "#![enable(implicit_some)]\n"] {
            let text = format!("{prefix}[(\"sword\", (damage: 1))]\n");
            assert_eq!(data_table_version(text.as_bytes()), Ok(Some(0)));
            let bytes = migrate_data_table(text.as_bytes(), 0).expect("migration");
            assert_eq!(
                data_table_version(&bytes),
                Ok(Some(regia_asset::data_table::DATA_TABLE_FORMAT_VERSION))
            );
        }
    }

    #[test]
    fn implicit_v1_cue_is_reported_as_v1_and_v2_as_2() {
        assert_eq!(read_implicit_v1(b"( name: \"impact\" )").expect("cue"), 1);
        assert_eq!(
            read_implicit_v1(b"( version: 2, name: \"impact\" )").expect("cue"),
            2
        );
    }

    #[test]
    fn music_v1_migrates_by_stamping_the_version() {
        let old = b"// the combat track\n( name: \"combat\", bpm: 132.0, stems: [] )";
        assert_eq!(read_implicit_v1(old).expect("music"), 1);
        let new = migrate_music(old, 1).expect("migrate");
        let text = String::from_utf8(new).expect("utf8");
        assert!(text.contains("// the combat track"), "{text}");
        assert_eq!(
            read_implicit_v1(text.as_bytes()).expect("music"),
            regia::audio::MUSIC_VERSION
        );
        let def = regia::audio::MusicDef::from_ron(&text).expect("v2 reads");
        assert_eq!(def.version, regia::audio::MUSIC_VERSION);
        assert_eq!(def.name, "combat");
        assert!(migrate_music(b"[1, (2)]", 1).is_err());
    }

    /// ADR-7226: an `.audiopatch.ron` v1 file is checked by the loader and re-stamped as v2, its
    /// comments and layout kept; a v1 file that already uses a v2 field is refused untouched.
    #[test]
    fn audiopatch_v1_upgrades_to_v2_keeping_comments() {
        let old = "// the hover whine\n(\n    version: 1,\n    name: \"whine\",\n    parameters: [],\n    \
                   layers: [(name: \"w\", sound: Some(\"w.ogg\"))],\n)";
        let new = migrate_patch(old.as_bytes(), 1).expect("migrate");
        let text = String::from_utf8(new).expect("utf8");
        assert_eq!(text, old.replace("version: 1", "version: 2"));
        let def = regia::audio::AudioPatchDef::from_ron(&text).expect("v2 reads");
        assert_eq!(def.version, regia::audio::PATCH_VERSION);
        let wrong = old.replace(
            "sound: Some(\"w.ogg\")",
            "one_shot: Some((sounds: [\"w.ogg\"], trigger: Manual))",
        );
        assert!(
            migrate_patch(wrong.as_bytes(), 1).is_err(),
            "v2 field in a v1 file"
        );
        assert!(migrate_patch(b"[1, (2)]", 1).is_err());
    }

    #[test]
    fn cue_v1_migrates_by_stamping_the_version() {
        let old = b"// kept\n( name: \"impact\", sounds: [] )";
        let new = migrate_cue(old, 1).expect("migrate");
        let text = String::from_utf8(new).expect("utf8");
        assert!(text.starts_with("// kept\n("), "{text}");
        // The migration leaves the file at the current cue version (3 since ADR-1697).
        assert_eq!(
            read_implicit_v1(text.as_bytes()).expect("cue"),
            regia::audio::CUE_VERSION
        );
        assert!(text.contains("name: \"impact\""));
    }

    /// ADR-1697: a v2 cue (it already has a `version` field) is re-stamped in place, comments kept.
    #[test]
    fn cue_v2_migrates_by_rewriting_the_version_number() {
        let old = b"// kept\n( version: 2, name: \"impact\", sounds: [] )";
        let new = migrate_cue(old, 2).expect("migrate");
        let text = String::from_utf8(new).expect("utf8");
        assert_eq!(
            text,
            format!(
                "// kept\n( version: {}, name: \"impact\", sounds: [] )",
                regia::audio::CUE_VERSION
            )
        );
        assert_eq!(
            read_implicit_v1(text.as_bytes()).expect("cue"),
            regia::audio::CUE_VERSION
        );
    }

    #[test]
    fn cue_migration_ignores_parens_in_comments_and_finds_the_root() {
        let old = "/* a (note) */ // (more)\nSoundCueDef( name: \"a(b\", sounds: [] ) // tail (x)";
        let new = migrate_cue(old.as_bytes(), 1).expect("migrate");
        let text = String::from_utf8(new).expect("utf8");
        let want = regia::audio::CUE_VERSION;
        assert!(
            text.contains(&format!("SoundCueDef(\n    version: {want},")),
            "{text}"
        );
        assert!(text.starts_with("/* a (note) */ // (more)\n"), "{text}");
        assert!(regia_core::version::peek_ron_version(&text).is_ok_and(|v| v == want));
        assert!(migrate_cue(b"[1, (2)]", 1).is_err());
    }

    #[test]
    fn animgraph_v1_upgrades_to_the_current_version() {
        let formats = formats();
        let anim = formats.iter().find(|f| f.name == "animgraph").expect("animgraph");
        assert_eq!(anim.current, regia_animation::ANIMGRAPH_FORMAT_VERSION);
        let v1 = br#"(format_version: 1, parameters: [("Speed", Float(0.0))], root: Clip((clip: "Run", looping: true, speed: 1.0, sync_group: None)))"#;
        let out = (anim.migrate)(v1, 1).expect("migrate");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains(&format!("format_version: {}", anim.current)),
            "{text}"
        );
    }

    #[test]
    fn behavior_tree_v1_upgrades_with_a_byte_exact_backup_and_is_not_planned_twice() {
        let project = regia_test_utils::unique_temp_dir("upgrade-bt-reactive");
        let assets = project.join("assets");
        std::fs::create_dir_all(&assets).expect("assets");
        let file = assets.join("guard.bt.ron");
        let v1 = "// guard\n(\n    root: 0,\n    nodes: [\n        (name: \"ready\", kind: condition, children: [], key: \"ready\", seconds: 0.0, times: None, task: \"\", policy: require_all),\n    ],\n)\n";
        std::fs::write(&file, v1).expect("v1");
        let formats = formats();
        let before = plan_with(&project, &formats).expect("plan");
        assert_eq!(before.len(), 1);
        assert_eq!(
            (before[0].format, before[0].found, before[0].current),
            ("bt", 1, 2)
        );
        run_with(
            &UpgradeProjectArgs {
                project: project.clone(),
                write: true,
                engine: None,
                json: false,
                backup: None,
            },
            &formats,
        )
        .expect("upgrade");
        let migrated = std::fs::read_to_string(&file).expect("v2");
        assert_eq!(
            migrated,
            regia_ai::migrate_bt_ron(v1).expect("expected migration")
        );
        assert_eq!(
            std::fs::read_to_string(assets.join("guard.bt.ron.bak")).expect("backup"),
            v1
        );
        let mut old = regia_ai::bt_edit::parse_bt_ron(v1).expect("old definition");
        old.format_version = 2;
        assert_eq!(
            regia_ai::bt_edit::parse_bt_ron(&migrated).expect("new definition"),
            old
        );
        let after = plan_with(&project, &formats).expect("second plan");
        assert_eq!(after.len(), 1);
        assert_eq!((after[0].found, after[0].current), (2, 2));
        std::fs::remove_dir_all(&project).expect("cleans project");
    }

    /// ADR-8900 (format batch 2): `.logic.ron` and `.uianim.ron` v1 upgrade to v2 by rewriting only
    /// the version number (a uianim file without the field is re-saved at v2); the files load with
    /// the same meaning; a second `--write` changes no byte; the loaders still check the file.
    #[test]
    fn logic_and_uianim_v1_upgrade_to_v2_and_a_second_write_changes_nothing() {
        let project = regia_test_utils::unique_temp_dir("upgrade-logic-uianim-v2");
        let assets = project.join("assets");
        std::fs::create_dir_all(&assets).expect("assets");
        let logic_v1 = include_str!("../../../regia_agent/tests/fixtures/format-corpus/logic/v1.ron");
        let uianim_v1 = include_str!("../../../regia_agent/tests/fixtures/format-corpus/uianim/v1.ron");
        let bare = "// no version field\n(animations: { \"a\": (tracks: [(property: \"opacity\", keys: [(time: 0.0, value: 1.0)])]) })\n";
        std::fs::write(assets.join("door.logic.ron"), logic_v1).expect("logic");
        std::fs::write(assets.join("hud.uianim.ron"), uianim_v1).expect("uianim");
        std::fs::write(assets.join("bare.uianim.ron"), bare).expect("bare");
        let formats = formats();
        let before = plan_with(&project, &formats).expect("plan");
        let mut found: Vec<(&str, u32, u32)> =
            before.iter().map(|p| (p.format, p.found, p.current)).collect();
        found.sort_unstable();
        assert_eq!(found, [("logic", 1, 2), ("uianim", 1, 2), ("uianim", 1, 2)]);
        let args = UpgradeProjectArgs {
            project: project.clone(),
            write: true,
            engine: None,
            json: false,
            backup: None,
        };
        run_with(&args, &formats).expect("upgrade");
        let read = |name: &str| std::fs::read_to_string(assets.join(name)).expect("reads");
        let (logic, hud, bare_out) = (
            read("door.logic.ron"),
            read("hud.uianim.ron"),
            read("bare.uianim.ron"),
        );
        assert_eq!(
            logic,
            logic_v1.replacen("format_version: 1", "format_version: 2", 1),
            "only the number changed"
        );
        assert_eq!(
            hud,
            uianim_v1.replacen("format_version: 1", "format_version: 2", 1),
            "only the number changed"
        );
        assert!(bare_out.contains("format_version: 2"), "{bare_out}");
        assert_eq!(
            regia_ai::LogicGraphDef::from_ron(&logic).expect("v2 loads"),
            regia_ai::LogicGraphDef::from_ron(logic_v1).expect("v1 loads")
        );
        let lib = |t: &str| regia_ui::anim_timeline::UiAnimLibrary::from_ron(t).expect("loads");
        assert_eq!(lib(&hud), lib(uianim_v1));
        assert_eq!(lib(&bare_out), lib(bare));
        // Idempotent: the second run plans nothing and rewrites no byte.
        let after = plan_with(&project, &formats).expect("second plan");
        assert!(
            after.iter().all(|p| p.found == p.current),
            "nothing left to migrate"
        );
        run_with(&args, &formats).expect("second upgrade");
        assert_eq!(read("door.logic.ron"), logic);
        assert_eq!(read("hud.uianim.ron"), hud);
        assert_eq!(read("bare.uianim.ron"), bare_out);
        // The loaders still check what they rewrite.
        assert!(migrate_logic(b"(format_version: 1, rules: [(name: \"\", event: Wobble)])", 1).is_err());
        assert!(migrate_uianim(b"(format_version: 1, animations: { \"a\": (tracks: []) })", 1).is_err());
        assert!(
            migrate_logic(b"(format_version: 1)", 7).is_err(),
            "no step from v7"
        );
        std::fs::remove_dir_all(&project).expect("cleans project");
    }

    /// ADR-1645: a v1 montage re-saves at v2 with the same content.
    #[test]
    fn montage_v1_upgrades_to_v2() {
        let formats = formats();
        let montage = formats.iter().find(|f| f.name == "montage").expect("montage");
        assert_eq!(montage.current, 2);
        let v1 = br#"(format_version: 1, name: "m", tracks: [(slot: "s", segments: [(clip: "c")])])"#;
        let out = (montage.migrate)(v1, 1).expect("migrate");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("format_version: 2"), "{text}");
    }

    /// ADR-7586: `<map>.viscells.ron` is a known v1 format: a file without `version` is v1 (current,
    /// nothing to do), a newer one is reported, and an unreadable one is an error, never "v1".
    #[test]
    fn upgrade_project_knows_viscells_v1() {
        let formats = formats();
        let viscells = formats
            .iter()
            .find(|f| f.name == "viscells")
            .expect("viscells row");
        assert_eq!(viscells.suffixes, &[".viscells.ron"]);
        assert_eq!(viscells.current, regia_render::viscells::VISCELLS_FORMAT_VERSION);
        assert_eq!(viscells_file_version(b"(cells: [])").expect("version"), Some(1));
        assert_eq!(
            viscells_file_version(b"(version: 2, cells: [])").expect("version"),
            Some(2)
        );
        assert!(viscells_file_version(b"not ron at all [").is_err());
    }

    /// ADR-8512: `.hoverhull.ron` is a known v1 format. A current file needs no rewrite, a newer
    /// one is reported, and text that is not a map is an error.
    #[test]
    fn upgrade_project_knows_hoverhull() {
        let formats = formats();
        let hull = formats
            .iter()
            .find(|f| f.name == "hoverhull")
            .expect("hoverhull row");
        assert_eq!(hull.suffixes, &[".hoverhull.ron"]);
        assert_eq!(
            hull.current,
            regia::framework::det_hover::HOVERHULL_FORMAT_VERSION
        );
        let v1 = b"(\n    format_version: 1,\n    name: \"default\",\n)\n";
        assert_eq!(read_ron_version(v1).expect("v1"), 1);
        assert_eq!(
            read_ron_version(b"(format_version: 2, name: \"x\")").expect("v2"),
            2
        );
        assert!(read_ron_version(b"not ron at all [").is_err());
        assert!((hull.migrate)(v1, 0).is_err());
    }

    /// ADR-8694: `.rules.ron` is a known v1 format. A current file passes unchanged after its check, a
    /// newer one is refused naming its version, and a file whose check fails is an error.
    #[test]
    fn upgrade_project_knows_rules_ron() {
        let formats = formats();
        let rules = formats
            .iter()
            .find(|f| f.name == "rules-ron")
            .expect("rules-ron row");
        assert_eq!(rules.suffixes, &[".rules.ron"]);
        assert_eq!(
            rules.current,
            regia::framework::rules::ruleset::RULES_RON_FORMAT_VERSION
        );
        let v1 = include_bytes!("../../../regia_agent/tests/fixtures/format-corpus/rules-ron/v1.ron");
        assert_eq!(read_ron_version(v1).expect("v1"), 1);
        assert_eq!((rules.migrate)(v1, 1).expect("v1 stays"), v1.to_vec());
        let v2 = b"(format_version: 2, id: \"x\", skeleton: \"ctf\")";
        assert_eq!(read_ron_version(v2).expect("v2"), 2);
        let err = (rules.migrate)(v2, 2).expect_err("v2 is refused");
        assert!(err.contains("no migrator from rules.ron v2"), "{err}");
        let bad = b"(format_version: 1, id: \"Bad Id\", skeleton: \"ctf\")";
        assert!(
            (rules.migrate)(bad, 1)
                .expect_err("bad id")
                .contains("ruleset_invalid")
        );
    }

    /// ADR-8630: `.bike.ron` is a known v1 format. A current file needs no rewrite, a newer one is
    /// reported, and the v1 migrator writes the canonical text.
    #[test]
    fn upgrade_project_knows_bike() {
        let formats = formats();
        let bike = formats.iter().find(|f| f.name == "bike").expect("bike row");
        assert_eq!(bike.suffixes, &[".bike.ron"]);
        assert_eq!(
            bike.current,
            regia::physics::vehicle_bike::asset::BIKE_FORMAT_VERSION
        );
        let v1 = b"(\n    format_version: 1,\n    name: \"city\",\n)\n";
        assert_eq!(read_ron_version(v1).expect("v1"), 1);
        assert_eq!(
            read_ron_version(b"(format_version: 2, name: \"x\")").expect("v2"),
            2
        );
        assert!((bike.migrate)(v1, 0).is_err());
    }

    /// ADR-8639: the two terrain source descriptors are listed at v1 and a `--write` run leaves
    /// them byte for byte (nothing to migrate, no backup); a newer file reports its own version
    /// (the key is read before the typed parse, so an unknown field does not hide it), and a file
    /// without the key is an error, never "v1".
    #[test]
    fn upgrade_project_lists_terrain_source_descriptors_at_v1_and_leaves_them() {
        let dir = regia_test_utils::unique_temp_dir("upgrade-terrain-source-descriptors");
        let terrain = dir.join("maps/basin");
        std::fs::create_dir_all(&terrain).expect("terrain dir");
        let raster = br#"{"raster_descriptor":1,"spacing_m":[1.0,1.0],"registration":"center"}"#;
        let tiles = br#"{"terrain_source_tiles":1,"grid":{"cols":3,"rows":2,"cell_m":1.0,"sample0_m":[0.0,0.0]},"units":{"meters_per_count":0.01,"zero_counts":0},"tiles":[{"path":"a.png","origin":[0,0],"size":[3,2]}]}"#;
        std::fs::write(terrain.join("heightfield.raster.json"), raster).expect("raster");
        std::fs::write(terrain.join("heightfield.tiles.json"), tiles).expect("tiles");

        let plan = plan_with(&dir, &formats()).expect("plan");
        let found = |format: &str| -> Vec<(String, u32, u32)> {
            plan.iter()
                .filter(|p| p.format == format)
                .map(|p| (p.path.to_string_lossy().replace('\\', "/"), p.found, p.current))
                .collect()
        };
        assert_eq!(
            found("terrain-raster-descriptor"),
            [("maps/basin/heightfield.raster.json".to_string(), 1, 1)]
        );
        assert_eq!(
            found("terrain-source-tiles"),
            [("maps/basin/heightfield.tiles.json".to_string(), 1, 1)]
        );
        assert!(plan.iter().all(|p| !p.needs_migration()));

        let args = UpgradeProjectArgs {
            project: dir.clone(),
            write: true,
            engine: None,
            json: false,
            backup: None,
        };
        run_with(&args, &formats()).expect("--write with only current descriptors");
        assert_eq!(
            std::fs::read(terrain.join("heightfield.raster.json")).expect("raster"),
            raster
        );
        assert_eq!(
            std::fs::read(terrain.join("heightfield.tiles.json")).expect("tiles"),
            tiles
        );
        assert!(!terrain.join("heightfield.raster.json.bak").exists());
        assert!(!terrain.join("heightfield.tiles.json.bak").exists());

        assert_eq!(
            terrain_raster_descriptor_version(br#"{"raster_descriptor":2,"later":true}"#).expect("probe"),
            Some(2)
        );
        assert_eq!(
            terrain_source_tiles_version(br#"{"terrain_source_tiles":2,"later":true}"#).expect("probe"),
            Some(2)
        );
        assert!(terrain_raster_descriptor_version(br#"{"spacing_m":[1.0,1.0]}"#).is_err());
        assert!(terrain_source_tiles_version(b"not json").is_err());
    }

    /// ADR-1595: a v1 holes file (no `version` field) re-saves at v2 with the same shapes.
    #[test]
    fn holes_v1_upgrades_to_v2() {
        let formats = formats();
        let holes = formats.iter().find(|f| f.name == "terrain-holes").expect("holes");
        let v1 = b"(shapes: [rect(center: (1.0, 2.0), half_extents: (3.0, 4.0), rotation_radians: 0.0)])";
        assert_eq!(holes_file_version(v1).expect("version"), Some(1));
        let out = (holes.migrate)(v1, 1).expect("migrate");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("version: 2"), "{text}");
        assert_eq!(holes_file_version(text.as_bytes()).expect("version"), Some(2));
        let mask = regia_terrain::HolesMask::from_holes_ron(&text).expect("v2 loads");
        assert_eq!(mask.shapes.len(), 1);
        assert!(mask.named.is_empty());
    }

    #[test]
    fn material_v2_upgrades_to_v3_with_glass_defaults() {
        let formats = formats();
        let material = formats.iter().find(|f| f.name == "material").expect("material");
        let v2 = br#"(
            format_version: 2,
            shader: Standard,
            base: (roughness: 0.4),
            hard_surface: (),
        )"#;
        let out = (material.migrate)(v2, 2).expect("migrate");
        let text = String::from_utf8(out).expect("utf8");
        let current = format!("format_version: {}", regia_asset::MATERIAL_FORMAT_VERSION);
        assert!(text.contains(&current), "{text}");
        assert!(text.contains("emissive_strength: 1.0"), "{text}");
        assert!(text.contains("orm_packed: true"), "{text}");
        assert!(text.contains("alpha_cutoff: 0.5"), "{text}");
        assert!(
            !text.contains("transmission"),
            "no transmission block on an opaque v2 material: {text}"
        );
    }

    #[test]
    fn material_v4_upgrades_to_v5_with_the_default_weather() {
        let formats = formats();
        let material = formats.iter().find(|f| f.name == "material").expect("material");
        let v4 = br#"(
            format_version: 4,
            shader: Standard,
            base: (roughness: 0.8),
            thin_film: (factor: 0.5),
        )"#;
        let out = (material.migrate)(v4, 4).expect("migrate");
        let text = String::from_utf8(out).expect("utf8");
        let current = format!("format_version: {}", regia_asset::MATERIAL_FORMAT_VERSION);
        assert!(text.contains(&current), "{text}");
        assert!(text.contains("factor: 0.5"), "the film is kept: {text}");
        assert!(
            !text.contains("weather"),
            "the default response writes no block: {text}"
        );
    }

    #[test]
    fn material_v1_upgrades_to_current() {
        let formats = formats();
        let material = formats.iter().find(|f| f.name == "material").expect("material");
        assert_eq!(material.current, regia_asset::MATERIAL_FORMAT_VERSION);
        let v1 = br#"(
            format_version: 1,
            shader: Standard,
            base: (),
        )"#;
        let out = (material.migrate)(v1, 1).expect("migrate");
        let text = String::from_utf8(out).expect("utf8");
        let current = format!("format_version: {}", regia_asset::MATERIAL_FORMAT_VERSION);
        assert!(text.contains(&current), "{text}");
        assert!(text.contains("ior: 1.5"), "{text}");
        assert!(text.contains("multi_scatter: true"), "{text}");
        assert!(text.contains("specular_occlusion: true"), "{text}");
    }

    /// Every format past its first version has a real migrator from each older version
    /// (ADR-1378). The registry reads `current` from the format crates, so a bump that leaves
    /// [`no_migration`] in place fails here, not in a user's `--write`.
    #[test]
    fn every_format_with_older_versions_has_a_migrator() {
        // Theme and bake files start at v0 (no version field); the deterministic navmesh enters at
        // DO27's v2 (no Regia v1 file exists, ADR-8174); every other format at v1.
        let first = |name: &str| match name {
            "theme" | "bake" | "prefab-variant" | "profile-capture" | "audio-buses" => 0,
            "nav-det-navmesh" => regia_nav::det::navmesh::NAVMESH_VERSION,
            _ => 1,
        };
        let mut gaps = Vec::new();
        for format in formats() {
            for from in first(format.name)..format.current {
                // Empty bytes: a real migrator fails on them with its own parse message.
                if let Err(e) = (format.migrate)(b"", from)
                    && e.starts_with("no migrator registered")
                {
                    gaps.push(format!("{} v{from} -> v{}", format.name, format.current));
                }
            }
        }
        assert!(gaps.is_empty(), "formats without a migrator: {gaps:?}");
    }

    /// The registry's `current` is the format crate's constant for every format the crates name
    /// (ADR-1378: `cue` and the `v1` entries used to be literals).
    #[test]
    fn registry_versions_are_the_format_crates_constants() {
        let expected = [
            ("meta", regia_asset::META_FORMAT_VERSION),
            ("material", regia_asset::MATERIAL_FORMAT_VERSION),
            ("scene-level", regia_scene::format::SCENE_FORMAT_VERSION),
            ("scene-prefab", regia_scene::format::SCENE_FORMAT_VERSION),
            ("ui", regia_ui::UI_RON_VERSION),
            ("uisettings", regia_ui::UI_SETTINGS_VERSION),
            ("glyphs", regia_ui::INPUT_GLYPHS_VERSION),
            ("theme", regia_ui::THEME_FORMAT_VERSION),
            ("audio-buses", regia::audio::AUDIO_BUSES_FORMAT_VERSION),
            ("vfx", regia_vfx::VFX_FORMAT_VERSION),
            ("glasscrack", regia_asset::glasscrack::GLASSCRACK_FORMAT_VERSION),
            ("loopgen", regia::audio::LOOPGEN_VERSION),
            ("video", regia_media::source::VIDEO_SOURCE_VERSION),
            ("videolist", regia_media::source::VIDEO_LIST_VERSION),
            ("matgraph", regia_matgraph::MATGRAPH_FORMAT_VERSION),
            ("matinst", regia_matgraph::MATINST_FORMAT_VERSION),
            ("matfunc", regia_matgraph::MATFUNC_FORMAT_VERSION),
            ("animgraph", regia_animation::ANIMGRAPH_FORMAT_VERSION),
            ("animedit", regia_animation::ANIMEDIT_FORMAT_VERSION),
            ("spring", regia_animation::SPRING_FORMAT_VERSION),
            ("drivers", regia_animation::DRIVERS_FORMAT_VERSION),
            ("skelprofile", regia_animation::SKELPROFILE_FORMAT_VERSION),
            ("cue", regia::audio::CUE_VERSION),
            ("music", regia::audio::MUSIC_VERSION),
            ("audiopatch", regia::audio::PATCH_VERSION),
            ("ambience", regia::audio::AMBIENCE_VERSION),
            (
                "terrain-layers",
                regia_terrain::layers::TERRAIN_LAYERS_FORMAT_VERSION,
            ),
            ("terrain-holes", regia_terrain::HOLES_FORMAT_VERSION),
            ("scatter", regia_terrain::SCATTER_FORMAT_VERSION),
            ("scatter-paint", regia_terrain::SCATTER_PAINT_FORMAT_VERSION),
            ("grass", regia_terrain::GRASS_FORMAT_VERSION),
            ("tilemap", regia::two_d::TILEMAP_FORMAT_VERSION),
            ("atlas", regia::two_d::ATLAS_FORMAT_VERSION),
            ("autotile", regia::two_d::AUTOTILE_FORMAT_VERSION),
            (
                "tilepatterns",
                regia::two_d::tilepatterns::TILEPATTERNS_FORMAT_VERSION,
            ),
            ("shots", regia::framework::shots::SHOT_SEQUENCE_FORMAT_VERSION),
            ("sequence", regia::framework::sequencer::SEQUENCE_FORMAT_VERSION),
            ("save", regia::framework::save_game::SAVE_FORMAT_VERSION),
            ("pak", regia_asset::FPAK_VERSION),
            ("cooked-mesh", regia_asset::COOKED_MESH_FORMAT_VERSION),
            ("bake", regia_pt::bake_desc::BAKE_DESC_FORMAT_VERSION),
            ("cooked-clip", regia_animation::clip::CLIP_FORMAT_VERSION),
            ("lightmap", regia_asset::lightmap::LIGHTMAP_FORMAT_VERSION),
            ("montage", regia_animation::MONTAGE_FORMAT_VERSION),
            ("sync", regia_animation::SYNC_FORMAT_VERSION),
            ("slots", regia_animation::SLOTS_FORMAT_VERSION),
            ("montage-variant", regia_animation::MONTAGE_VARIANT_FORMAT_VERSION),
            ("render-job", regia_agent::render_job::RENDER_JOB_FORMAT_VERSION),
            ("shot", regia_agent::shot_list::SHOT_LIST_FORMAT_VERSION),
            ("cloth", regia::physics::cloth::asset::CLOTH_FORMAT_VERSION),
            ("soft", regia::physics::cloth::asset::SOFT_FORMAT_VERSION),
            (
                "cooked-cloth",
                regia::physics::cloth::asset::COOKED_CLOTH_FORMAT_VERSION,
            ),
            (
                "cooked-soft",
                regia::physics::cloth::asset::COOKED_SOFT_FORMAT_VERSION,
            ),
            (
                "destructible",
                regia::physics::destruction::DESTRUCTIBLE_FORMAT_VERSION,
            ),
            (
                "destructionfx",
                regia::framework::destruction_fx::DESTRUCTION_FX_FORMAT_VERSION,
            ),
            (
                "fracture",
                regia_asset_pipeline::fracture::FRACTURE_FORMAT_VERSION,
            ),
            ("vehicle", regia::physics::vehicle_asset::VEHICLE_FORMAT_VERSION),
            (
                "vehicle-snapshot",
                regia::physics::vehicle_snapshot::VEHICLE_SNAPSHOT_FORMAT_VERSION,
            ),
            ("physmat", regia::physics::material::PHYSMAT_FORMAT_VERSION),
            ("hull-set", regia::physics::hull_set::HULL_SET_FORMAT_VERSION),
            ("statetree", regia_ai::state_tree::STATE_TREE_FORMAT_VERSION),
            ("logic", regia_ai::logic::LOGIC_FORMAT_VERSION),
            (
                "smartobject",
                regia_ai::smart_object_def::SMART_OBJECT_DEF_VERSION,
            ),
            ("utility", regia_ai::utility::UTILITY_AI_FORMAT_VERSION),
            (
                "spatialquery",
                regia_ai::spatial_query::SPATIAL_QUERY_FORMAT_VERSION,
            ),
            ("camera", regia::framework::camera::asset::CAMERA_FORMAT_VERSION),
            ("voice-chat", regia::audio::voice_chat::CONFIG_FORMAT_VERSION),
            ("weather", regia_vfx::weather::WEATHER_CONFIG_FORMAT_VERSION),
            ("pgraph", regia_procgen::graph::PGRAPH_FORMAT_VERSION),
            ("pgvar", regia_procgen::graph::PGVAR_FORMAT_VERSION),
            ("pgout", regia_procgen::pgout::PGOUT_VERSION),
            (
                "curves",
                regia::framework::curve_table::CURVE_TABLE_FORMAT_VERSION,
            ),
            ("chooser", regia::framework::chooser::CHOOSER_FORMAT_VERSION),
            ("attrs", regia::framework::abilities::ATTRIBUTES_FORMAT_VERSION),
            ("effect", regia::framework::abilities::EFFECT_FORMAT_VERSION),
            ("ability", regia::framework::abilities::ABILITY_FORMAT_VERSION),
            ("biome", regia_procgen::biome::BIOME_FORMAT_VERSION),
            ("ragdoll", regia::framework::ragdoll::file::RAGDOLL_FORMAT_VERSION),
            (
                "motiondb",
                regia_animation::motion_matching::MOTIONDB_FORMAT_VERSION,
            ),
            (
                "motiondb-cooked",
                regia_animation::motion_matching::COOKED_MOTIONDB_VERSION,
            ),
            ("retarget", regia_animation::RETARGET_FORMAT_VERSION),
            ("navlinks", regia_nav::NAV_LINKS_FORMAT_VERSION),
            ("navlanes", regia_nav::NAV_LANES_FORMAT_VERSION),
            ("demo", regia::framework::demo::DEMO_FORMAT_VERSION),
            ("frec", regia::framework::input_recording::FREC_FORMAT_VERSION),
            ("netdemo", regia_net::recorder::format::NETDEMO_FORMAT_VERSION),
            ("netcapture", regia_net::capture::CAPTURE_FORMAT_VERSION),
            ("collision-set", regia_asset::COLLISION_SET_FORMAT_VERSION),
            ("prefab-instances", regia_asset::PREFAB_INSTANCES_FORMAT_VERSION),
            ("net-settings", regia_net::settings::NET_SETTINGS_FORMAT_VERSION),
            ("netprofile", regia_net::profile::NET_PROFILE_FORMAT_VERSION),
            (
                "server-toml",
                regia_net::server_host::config::SERVER_TOML_FORMAT_VERSION,
            ),
            ("bans", regia_net::session::bans::BAN_LIST_FORMAT_VERSION),
            (
                "net-manifest",
                regia_net::session::manifest::NET_MANIFEST_FORMAT_VERSION,
            ),
            ("server-key", regia_net::crypto::SERVER_KEY_FORMAT_VERSION),
            ("siminput", regia_net::session::hub::SIM_RECORDING_FORMAT_VERSION),
            ("desync", regia_net::desync::DESYNC_BUNDLE_FORMAT_VERSION),
            ("lsreplay", regia_net::lockstep::replay::LSREPLAY_VERSION),
            ("xr-config", regia_xr::XR_CONFIG_FORMAT_VERSION),
            ("xr-actions", regia_xr::input::MANIFEST_FORMAT_VERSION),
            ("xrsim", regia_xr::sim::SIM_FORMAT_VERSION),
            ("engine-release", regia_cli::distribution::RELEASE_FORMAT_VERSION),
            ("session", regia_cli::session::SESSION_FILE_VERSION),
            (
                "validation",
                regia_asset_pipeline::validation_rules::VALIDATION_FILE_FORMAT_VERSION,
            ),
            (
                "import-rules",
                regia_asset_pipeline::import_rules::IMPORT_RULES_FORMAT_VERSION,
            ),
            (
                "assetpkg",
                regia_asset_pipeline::asset_package::ASSET_PACKAGE_FORMAT_VERSION,
            ),
            ("uianim", regia_ui::anim_timeline::UIANIM_FORMAT_VERSION),
            ("data-asset", regia_asset::data_asset::DATA_ASSET_FORMAT_VERSION),
            ("asset-ids", regia_asset::asset_ids::ASSET_IDS_FORMAT_VERSION),
            ("data-table", regia_asset::data_table::DATA_TABLE_FORMAT_VERSION),
            ("asset-tags", regia_asset::registry::TAG_RULES_FORMAT_VERSION),
            (
                "asset-collections",
                regia_asset::asset_collections::ASSET_COLLECTIONS_FORMAT_VERSION,
            ),
            (
                "import-presets",
                regia_asset::import_presets::IMPORT_PRESETS_FORMAT_VERSION,
            ),
            ("asset-registry", regia_asset::registry::REGISTRY_FORMAT_VERSION),
            ("input-overrides", regia_input::OVERRIDES_VERSION),
            (
                "permutations",
                regia_render::permutation_policy::PERMUTATION_MANIFEST_FORMAT_VERSION,
            ),
            ("convc", regia::framework::conversation::cooked::CONVC_VERSION),
            ("bt", regia_ai::tree::BT_FORMAT_VERSION),
            ("terrain-weights", regia_terrain::LAYER_WEIGHTS_FORMAT_VERSION),
            (
                "terrain-index",
                regia_terrain::stream::TERRAIN_INDEX_FORMAT_VERSION,
            ),
            (
                "prefab-variant",
                regia_scene::prefab_variant::PREFAB_VARIANT_FORMAT_VERSION,
            ),
            (
                "profile-capture",
                regia_telemetry::profile_capture::PROFILE_CAPTURE_FORMAT_VERSION,
            ),
            ("world-index", regia_world::format::WORLD_INDEX_FORMAT_VERSION),
            ("ies-profile", regia_asset::ies::IES_PROFILE_FORMAT_VERSION),
            ("fluidcache", regia_fluid::cache::FLUIDCACHE_FORMAT_VERSION),
            ("known-hosts", regia_net::known_hosts::KNOWN_HOSTS_FORMAT_VERSION),
            (
                "provenance-policy",
                regia_asset_pipeline::provenance_policy::PROVENANCE_POLICY_FORMAT_VERSION,
            ),
            (
                "generator",
                regia_asset_pipeline::generators::GENERATOR_FORMAT_VERSION,
            ),
            (
                "texpack",
                regia_asset_pipeline::texture_pack::TEXPACK_FORMAT_VERSION,
            ),
            ("voxel-far", regia::render::visbuffer::VOXEL_FAR_FORMAT_VERSION),
            ("grass-density", regia_terrain::GRASS_DENSITY_FORMAT_VERSION),
            ("viewer-package", regia_viewer_pack::FORMAT_VERSION),
            ("viewer-env", regia_viewer::ENV_FORMAT_VERSION),
            ("jewelry-part-kit", regia_configurator::CONTRACT_MAJOR),
            ("jewelry-material-library", regia_configurator::CONTRACT_MAJOR),
            ("jewelry-build", regia_configurator::CONTRACT_MAJOR),
            ("jewelry-product-package", regia_configurator::CONTRACT_MAJOR),
            ("kind", regia_scene::kind::file::KIND_FORMAT_VERSION),
            ("baked-gi", regia_asset::baked_gi::BAKED_GI_VERSION),
            (
                "trailer",
                regia::physics::vehicle_trailer_asset::TRAILER_FORMAT_VERSION,
            ),
            ("aircraft", regia::physics::aero::asset::AIRCRAFT_FORMAT_VERSION),
            ("fcap", regia_asset::local_capture_bake::FCAP_FORMAT_VERSION),
            ("vehicle-audio", regia::audio::VEHICLE_AUDIO_VERSION),
            (
                "bake-checkpoint",
                regia_asset::bake_checkpoint::BAKE_CHECKPOINT_FORMAT_VERSION,
            ),
            (
                "hlod-voxels",
                regia_world::hlod_voxels::HLOD_VOXELS_FORMAT_VERSION,
            ),
            ("glossary", regia_ui::loc_glossary::GLOSSARY_FORMAT_VERSION),
            ("impactbank", regia::audio::impacts::IMPACT_BANK_FORMAT_VERSION),
            ("decal", regia_vfx::decal_format::DECAL_FORMAT_VERSION),
            (
                "lightmap-cells",
                regia_pt::lightmap::cells::CELL_BAKE_INDEX_VERSION,
            ),
            ("viscells", regia_render::viscells::VISCELLS_FORMAT_VERSION),
            (
                "launch-profiles",
                regia_asset_pipeline::launch_profiles::FORMAT_VERSION,
            ),
            (
                "level-variant",
                regia_scene::level_variant::LEVEL_VARIANT_FORMAT_VERSION,
            ),
            (
                "delivery-contract",
                regia_asset::delivery_contract::DELIVERY_CONTRACT_FORMAT_VERSION,
            ),
            ("terrain-editlayers", regia_terrain::EDIT_LAYERS_FORMAT_VERSION),
            ("nav-det-navmesh", regia_nav::det::navmesh::NAVMESH_VERSION),
            (
                "terrain-mesh-layers",
                regia_world_geom::meshlayers::MESH_LAYERS_FORMAT_VERSION,
            ),
            (
                "terrain-mesh-layers-cooked",
                regia_world_geom::cooked::COOKED_FORMAT_VERSION,
            ),
            ("ai-formation", regia_ai::FORMATION_FORMAT_VERSION),
            ("key-track", regia_cli::key_track::KEY_TRACK_FORMAT_VERSION),
            ("playlist-ron", regia_net::formats::PLAYLIST_RON_FORMAT_VERSION),
            (
                "terrain-index",
                regia_terrain::stream::TERRAIN_INDEX_FORMAT_VERSION,
            ),
            (
                "source-lint",
                regia_asset_pipeline::source_lint::SOURCE_LINT_POLICY_VERSION,
            ),
            ("release-chunks", regia_release_chunks::FORMAT_VERSION),
            ("hoverhull", regia::framework::det_hover::HOVERHULL_FORMAT_VERSION),
            ("bike", regia::physics::vehicle_bike::asset::BIKE_FORMAT_VERSION),
            ("ai-det-utility", regia_ai::det::DET_UTILITY_FORMAT_VERSION),
            (
                "terrain-raster-descriptor",
                regia_terrain::source::RASTER_DESCRIPTOR_VERSION,
            ),
            ("terrain-source-tiles", regia_terrain::source::TILE_SET_VERSION),
            (
                "rules-ron",
                regia::framework::rules::ruleset::RULES_RON_FORMAT_VERSION,
            ),
            ("hub-toml", regia_net::lobby::hub::HUB_TOML_FORMAT_VERSION),
            (
                "release-signature",
                regia_release_chunks::RELEASE_SIGNATURE_FORMAT,
            ),
            (
                "match-result",
                regia_framework::match_results::MATCH_RESULT_FORMAT_VERSION,
            ),
            ("gait", regia_animation::gait_asset::GAIT_FORMAT_VERSION),
        ];
        // The editor's own files (ADR-7902): the descriptor table is their constant here.
        let expected: Vec<(&str, u32)> = expected
            .into_iter()
            .chain(
                regia_core::editor_formats::EDITOR_FORMATS
                    .iter()
                    .map(|f| (f.name, f.version)),
            )
            .collect();
        let registry = formats();
        for &(name, version) in &expected {
            let format = registry
                .iter()
                .find(|f| f.name == name)
                .unwrap_or_else(|| panic!("{name} is not in the upgrade-project registry"));
            assert_eq!(format.current, version, "{name}");
        }
        let names: Vec<&str> = expected.iter().map(|(name, _)| *name).collect();
        let unlisted: Vec<&str> = registry
            .iter()
            .map(|f| f.name)
            .filter(|name| !names.contains(name))
            .collect();
        assert!(unlisted.is_empty(), "add these to the table above: {unlisted:?}");
    }

    #[test]
    fn editor_layout_v1_and_v2_upgrade_to_v3_by_the_number_only() {
        // ADR-8105 (a): panel instances are new and optional, every v1/v2 tab id is instance 0.
        let current = regia_core::editor_formats::editor_format("editor-layout")
            .expect("registered")
            .version;
        assert_eq!(current, 3);
        let v2 = "(\n    format_version: 2,\n    root: Tabs(tabs: [\"viewport\", \"outliner\"], active: 0),\n    floating: [(tabs: [\"problems\"], active: 0)],\n)\n";
        let out = String::from_utf8(migrate_editor_layout(v2.as_bytes(), 2).expect("v2")).expect("utf-8");
        assert_eq!(out, v2.replace("format_version: 2", "format_version: 3"));
        let v1 = v2.replace("format_version: 2", "format_version: 1");
        let out = String::from_utf8(migrate_editor_layout(v1.as_bytes(), 1).expect("v1")).expect("utf-8");
        assert_eq!(out, v2.replace("format_version: 2", "format_version: 3"));
        assert!(
            migrate_editor_layout(v2.as_bytes(), 3).is_err(),
            "nothing to do from v3"
        );
        assert!(
            migrate_editor_layout(b"(format_version: 2)", 2).is_err(),
            "not a layout"
        );
    }

    #[test]
    fn shots_v1_upgrade_to_v2_keeping_the_move() {
        let v1 = r#"(version: 1, shots: [(name: "s", duration: 1.0,
            camera: (path: (interp: Linear, keys: [(time: 0.0, position: (1.0, 2.0, 3.0))])))])"#;
        let out = migrate_shots(v1.as_bytes(), 1).expect("migrates");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains(&format!(
                "version: {}",
                regia::framework::shots::SHOT_SEQUENCE_FORMAT_VERSION
            )),
            "{text}"
        );
        assert!(
            !text.contains("rotation"),
            "an empty rotation track is not written: {text}"
        );
        let again = regia::framework::shots::ShotSequence::from_ron(&text).expect("re-parses");
        assert_eq!(again.shots[0].camera.path.keys[0].position, [1.0, 2.0, 3.0]);
        assert!(migrate_shots(b"\xff", 1).is_err());
    }

    /// w11-seq-preview-fixes item 4 (ADR-8640): a v2 `.shots.ron` (here with a Catmull-Rom tail
    /// key past its shot's end, as a v0.6 dev build wrote it) upgrades to v3, and upgrading the
    /// result again changes nothing.
    #[test]
    fn shots_v2_upgrade_to_v3_and_again_is_idempotent() {
        let v2 = r#"(version: 2, shots: [(name: "s", duration: 1.0,
            camera: (path: (keys: [(time: 0.0, position: (1.0, 2.0, 3.0)), (time: 2.0, position: (2.0, 2.0, 3.0))])))])"#;
        let out = migrate_shots(v2.as_bytes(), 2).expect("migrates");
        let text = String::from_utf8(out.clone()).expect("utf8");
        assert!(text.contains("version: 3"), "{text}");
        let again = migrate_shots(&out, 3).expect("migrates again");
        assert_eq!(again, out, "an upgraded file upgrades to the same bytes");
        let parsed = regia::framework::shots::ShotSequence::from_ron(&text).expect("re-parses");
        assert_eq!(
            parsed.shots[0].camera.path.keys[1].time, 2.0,
            "the tail key is kept"
        );
    }

    /// A hand-written `.variant.ron` without `format_version` plans as v0 and re-saves as v1
    /// (ADR-2323).
    #[test]
    fn a_versionless_prefab_variant_upgrades_to_v1() {
        let dir = regia_test_utils::unique_temp_dir("upgrade-variant");
        let v0 = r#"(base: (prefab: "8b6f2a11-0000-4000-8000-000000000001"))"#;
        std::fs::write(dir.join("red.variant.ron"), v0).expect("fixture");
        let plan = plan_with(&dir, &formats()).expect("plan");
        let found: Vec<&FilePlan> = plan.iter().filter(|p| p.format == "prefab-variant").collect();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].found, 0);
        assert!(found[0].needs_migration());
        let out = migrate_prefab_variant(v0.as_bytes(), 0).expect("migrates");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("format_version: 1"), "{text}");
        assert!(migrate_prefab_variant(b"(format_version: 9, base: 1)", 9).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `.theme.ron` v1 → v2 (ADR-8105 (e)): the plan finds the v1 file, the step rewrites only the
    /// `format_version` number (comments and layout kept), and the result loads as v2 with the same
    /// look; a v0 file still goes through the v0 migrator to the current version.
    #[test]
    fn a_v1_theme_upgrades_to_v2() {
        let v1 = include_str!("../../../regia_agent/tests/fixtures/format-corpus/theme/v1.ron");
        let dir = regia_test_utils::unique_temp_dir("upgrade-theme-v2");
        std::fs::write(dir.join("my-game.theme.ron"), v1).expect("fixture");
        let plan = plan_with(&dir, &formats()).expect("plan");
        let found: Vec<&FilePlan> = plan.iter().filter(|p| p.format == "theme").collect();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].found, 1);
        assert!(
            found[0].needs_migration(),
            "v1 is older than v{}",
            regia_ui::THEME_FORMAT_VERSION
        );
        let out = String::from_utf8(migrate_theme(v1.as_bytes(), 1).expect("migrates")).expect("utf8");
        assert_eq!(
            out,
            v1.replacen("format_version: 1,", "format_version: 2,", 1),
            "only the number changed"
        );
        let upgraded = regia_ui::parse_theme_ron(&out, "my-game.theme.ron").expect("v2 loads");
        let original = regia_ui::parse_theme_ron(v1, "my-game.theme.ron").expect("v1 loads");
        assert_eq!(upgraded.format_version, 2);
        assert_eq!(upgraded, original, "v1 reads as v2 with no rules");
        assert!(upgraded.rules.is_empty());
        assert!(
            migrate_theme(b"(format_version: 1, name: \"\")", 1).is_err(),
            "the loader still checks"
        );
        let v0 = migrate_theme(b"(name: \"old\")", 0).expect("v0 migrates");
        let v0 = String::from_utf8(v0).expect("utf8");
        assert!(v0.contains("format_version: 2"), "{v0}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tilemap_and_autotile_v1_upgrade_by_rewriting_only_the_version_number() {
        let map = "// keep me\n(version: 1, orientation: Orthogonal, width: 1, height: 1, tile_width: 8, tile_height: 8,\n \
            tilesets: [(name: \"t\", first_gid: 1, image: \"t.png\", image_width: 8, image_height: 8, tile_width: 8,\n \
            tile_height: 8, columns: 1, tile_count: 1)],\n layers: [Tiles((name: \"g\", data: [1]))])";
        let out = String::from_utf8(migrate_tilemap(map.as_bytes(), 1).expect("migrates")).expect("utf8");
        assert_eq!(
            out,
            map.replacen("(version: 1,", "(version: 2,", 1),
            "only the number changed"
        );
        let parsed = regia::two_d::tilemap::TileMapAsset::from_ron(&out).expect("re-parses");
        assert_eq!(parsed.version, regia::two_d::TILEMAP_FORMAT_VERSION);
        assert!(migrate_tilemap(b"\xff", 1).is_err());
        assert!(migrate_tilemap(b"(version: 1, width: 0)", 1).is_err());

        let rules = "(version: 1, tileset: \"t\", terrains: [(name: \"w\", layout: Edge16([0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]))])";
        let out = String::from_utf8(migrate_autotile(rules.as_bytes(), 1).expect("migrates")).expect("utf8");
        assert_eq!(out, rules.replacen("(version: 1,", "(version: 2,", 1));
        assert!(regia::two_d::AutoTileSet::from_ron(&out).is_ok());
        assert!(migrate_autotile(b"(version: 1, tileset: \"\", terrains: [])", 1).is_err());
    }

    #[test]
    fn sequence_and_render_job_upgrade_to_the_current_version_keeping_their_content() {
        // integ-v049a: the sequence files land on the current version (v5 since ADR-2948), not v4.
        let current = format!(
            "(version: {},",
            regia::framework::sequencer::SEQUENCE_FORMAT_VERSION
        );
        let v1 = r#"// The intro. version: 1 in a comment stays.
            (version: 1, name: "intro", tracks: [(name: "fade", target: "ui", property: "opacity",
            keys: Float([(time: 0.0, value: 0.0, ease: Smooth), (time: 2.0, value: 1.0)]))],
            markers: [(name: "go", time: 1.0)])"#;
        let text = String::from_utf8(migrate_sequence(v1.as_bytes(), 1).expect("migrates")).expect("utf8");
        assert!(text.contains(&current), "{text}");
        let again = regia::framework::sequencer::Sequence::from_ron(&text).expect("re-parses");
        assert_eq!(again.name, "intro");
        assert_eq!(again.markers[0].name, "go");
        assert!(!text.contains("tangent"), "v1 keys gain no tangent noise: {text}");
        assert_eq!(
            text,
            v1.replace("(version: 1,", &current),
            "only the number changed"
        );
        // v2 -> v3 (ADR-2405): a v2 file with sections keeps every byte but its version number.
        let v2 = r#"(version: 2, tracks: [
            (name: "music", keys: Audio([(sound: "theme", start: 0.0, end: 3.0, fade_out: 1.0)])),
            (name: "fx", keys: Sub([(sequence: "fx.seq.ron", start: 1.0, end: 2.0, speed: 2.0)])),
        ])"#;
        let text = String::from_utf8(migrate_sequence(v2.as_bytes(), 2).expect("migrates")).expect("utf8");
        assert_eq!(
            text,
            v2.replace("(version: 2,", &current),
            "only the number changed"
        );
        assert!(regia::framework::sequencer::Sequence::from_ron(&text).is_ok());
        // v3 -> v4 (ADR-2862): a v3 file keeps every byte but its version number, and the v4 file
        // round-trips, video sections and all.
        let v3 = r#"(version: 3, tracks: [
            (name: "music", keys: Audio([(sound: "theme", start: 0.0, end: 3.0, time_scale: 2.0)])),
            (name: "fx", keys: Sub([(sequence: "fx.seq.ron", start: 1.0, end: 2.0, repeat: Count(2))])),
        ])"#;
        let text = String::from_utf8(migrate_sequence(v3.as_bytes(), 3).expect("migrates")).expect("utf8");
        assert_eq!(
            text,
            v3.replace("(version: 3,", &current),
            "only the number changed"
        );
        let upgraded = regia::framework::sequencer::Sequence::from_ron(&text).expect("re-parses");
        let saved = upgraded.to_ron().expect("saves");
        let again = regia::framework::sequencer::Sequence::from_ron(&saved).expect("round trip");
        assert_eq!(again, upgraded);
        let with_video = r#"(version: 3, tracks: [
            (name: "wall", keys: Video([(video: "movies/a.webm", start: 0.0, end: 3.0, offset: 0.5)])),
        ])"#;
        let text =
            String::from_utf8(migrate_sequence(with_video.as_bytes(), 3).expect("migrates")).expect("utf8");
        assert!(text.contains(&current) && text.contains("Video"), "{text}");
        // v4 -> v5 (ADR-3197): a spatial section keeps the v4 sound, so the file is re-saved with
        // Doppler written off; a v4 file without one keeps every byte but its version number.
        let spatial = r#"(version: 4, tracks: [
            (name: "steps", keys: Audio([(sound: "step", start: 0.0, end: 2.0,
                spatial: Some((source: Binding("hero"))))])),
        ])"#;
        let text =
            String::from_utf8(migrate_sequence(spatial.as_bytes(), 4).expect("migrates")).expect("utf8");
        let resaved_version = format!(
            "version: {},",
            regia::framework::sequencer::SEQUENCE_FORMAT_VERSION
        );
        assert!(
            text.contains("doppler: 0.0") && text.contains(&resaved_version),
            "{text}"
        );
        let upgraded = regia::framework::sequencer::Sequence::from_ron(&text).expect("re-parses");
        let Some(regia::framework::sequencer::Channel::Audio(sounds)) =
            upgraded.tracks.first().map(|t| &t.keys)
        else {
            panic!("audio track")
        };
        assert_eq!(sounds[0].spatial.as_ref().map(|s| s.doppler), Some(0.0));
        let flat = r#"(version: 4, tracks: [
            (name: "music", keys: Audio([(sound: "theme", start: 0.0, end: 3.0)])),
        ])"#;
        let text = String::from_utf8(migrate_sequence(flat.as_bytes(), 4).expect("migrates")).expect("utf8");
        assert_eq!(
            text,
            flat.replace("(version: 4,", &current),
            "only the number changed"
        );
        // v5 → v6 (ADR-8105): only the number changes.
        let v5 = r#"(version: 5, tracks: [
            (name: "fade", target: "ui", property: "opacity", keys: Float([(time: 0.0, value: 0.0)])),
        ])"#;
        let text = String::from_utf8(migrate_sequence(v5.as_bytes(), 5).expect("migrates")).expect("utf8");
        assert_eq!(
            text,
            v5.replace("(version: 5,", &current),
            "only the number changed"
        );
        assert!(migrate_sequence(b"\xff", 1).is_err());
        assert!(migrate_sequence(b"(version: 1, tracks: [(name: \"\")])", 1).is_err());

        let job = r#"(version: 1, out: "renders", takes: [(name: "a", range: Frames(start: 0, end: 4))])"#;
        let text = String::from_utf8(migrate_render_job(job.as_bytes(), 1).expect("migrates")).expect("utf8");
        assert!(text.contains("version: 4"), "{text}");
        let again = regia_agent::render_job::RenderJob::from_ron(&text).expect("re-parses");
        assert_eq!(again.takes[0].name, "a");
        assert_eq!(again.product, None, "v4's product block is optional");
    }

    #[test]
    fn retarget_v1_upgrades_to_v2_with_no_goals() {
        let v1 = "(format_version: 1, name: \"guard\", translation_scale: Fixed(1.5))";
        let text = String::from_utf8(migrate_retarget(v1.as_bytes(), 1).expect("migrates")).expect("utf8");
        assert!(text.contains("format_version: 2"), "{text}");
        let upgraded = regia_animation::RetargetSetup::from_ron_str(&text).expect("re-parses");
        assert_eq!(
            upgraded,
            regia_animation::RetargetSetup::from_ron_str(v1).expect("parses")
        );
        assert!(upgraded.ik_goals.is_empty());
        assert!(migrate_retarget(b"(format_version: 3)", 3).is_err());
        assert!(migrate_retarget(b"\xff", 1).is_err());
    }

    #[test]
    fn motiondb_v1_upgrades_to_v2_unchanged() {
        let v1 = "(format_version: 1, skeleton: \"s\", schema: (root_joint: \"root\", feet: [\"l\"]), \
                  entries: [(clip: \"c\", looping: true)])";
        let text = String::from_utf8(migrate_motiondb(v1.as_bytes(), 1).expect("migrates")).expect("utf8");
        assert!(text.contains("format_version: 2"), "{text}");
        let upgraded =
            regia_animation::motion_matching::MotionDatabaseSource::from_ron_str(&text).expect("re-parses");
        let original =
            regia_animation::motion_matching::MotionDatabaseSource::from_ron_str(v1).expect("parses");
        assert_eq!(upgraded, original);
        assert_eq!(upgraded.schema.contacts, None);
        assert!(migrate_motiondb(b"\xff", 1).is_err());
    }

    #[test]
    fn ragdoll_v1_upgrades_to_v2_with_capsule_bodies() {
        let v1 = "(format_version: 1, forward: (0.0, 0.0, -1.0), bones: [\
                  (joint: \"hips\", radius: 0.12, mass: 10.6, length: None, limits: None)])";
        let text = String::from_utf8(migrate_ragdoll(v1.as_bytes(), 1).expect("migrates")).expect("utf8");
        // v1 migrates through v2 and v3 to the current version (4 since ADR-2963).
        assert!(text.contains("format_version: 4"), "{text}");
        assert!(text.contains("shape: Capsule"), "{text}");
        let profile = regia::framework::ragdoll::RagdollProfile::from_ron(&text).expect("re-parses");
        assert_eq!(profile.bones.len(), 1);
        assert_eq!(
            profile.bones[0].shape,
            regia::framework::ragdoll::RagdollShape::Capsule
        );
        assert!(migrate_ragdoll(b"\xff", 1).is_err());
        assert!(
            migrate_ragdoll(
                b"(format_version: 1, forward: (0.0, 0.0, -1.0), bones: [(joint: \"\")])",
                1
            )
            .is_err()
        );
    }

    #[test]
    fn ragdoll_v2_upgrades_to_v3_with_zero_gains() {
        let v2 = "(format_version: 2, forward: (0.0, 0.0, -1.0), bones: [\
                  (joint: \"hips\", radius: 0.12, mass: 10.6, length: None, limits: None, shape: Sphere)])";
        let text = String::from_utf8(migrate_ragdoll(v2.as_bytes(), 2).expect("migrates")).expect("utf8");
        assert!(text.contains("format_version: 4"), "{text}");
        assert!(
            !text.contains("active"),
            "zero gains write no active block: {text}"
        );
        let profile = regia::framework::ragdoll::RagdollProfile::from_ron(&text).expect("re-parses");
        assert!(!profile.active.drives());
        assert_eq!(
            profile.bones[0].shape,
            regia::framework::ragdoll::RagdollShape::Sphere
        );
        assert_eq!(
            migrate_ragdoll(text.as_bytes(), 3).map(String::from_utf8),
            Ok(Ok(text.clone())),
            "idempotent"
        );
    }

    #[test]
    fn navlanes_v1_upgrades_to_v2_keeping_its_text() {
        let v1 = "// Two lanes. format_version: 1 in a comment stays.\n(format_version: 1, lanes: [\
                  (points: [(0.0, 0.0, 0.0), (5.0, 0.0, 0.0)], next: [1]), (points: [(5.0, 0.0, 0.0), (5.0, 0.0, 5.0)])])";
        let text = String::from_utf8(migrate_navlanes(v1.as_bytes(), 1).expect("migrates")).expect("utf8");
        assert_eq!(text, v1.replace("(format_version: 1,", "(format_version: 2,"));
        let set = regia_nav::NavLaneSet::parse(&text).expect("re-parses");
        assert_eq!(set.format_version, 2);
        assert_eq!(set.lanes[0].weight, 1.0);
        assert_eq!(set.lanes[0].opposite, None);
        assert!(migrate_navlanes(b"\xff", 1).is_err());
        assert!(migrate_navlanes(b"(format_version: 1, lanes: [(points: [(0.0, 0.0, 0.0)])])", 1).is_err());
    }

    #[test]
    fn whole_name_suffixes_own_only_that_file_name() {
        assert!(owns_name("/net.ron", "net.ron"));
        assert!(!owns_name("/net.ron", "cabinet.ron"));
        assert!(!owns_name("/net.ron", "net.ron.bak"));
        assert!(owns_name(".bt.ron", "guard.bt.ron"));
        // Every whole-name entry of the sweep (ADR-2316) is spelled with the `/` prefix.
        let registry = formats();
        for name in [
            "net-settings",
            "server-toml",
            "bans",
            "xr-config",
            "validation",
            "asset-tags",
            "permutations",
        ] {
            let format = registry.iter().find(|f| f.name == name).expect("registered");
            assert!(
                format.suffixes.iter().all(|s| s.starts_with('/')),
                "{name}: {:?}",
                format.suffixes
            );
        }
    }

    #[test]
    fn upgrade_project_registry_migrates_both_network_configurations() {
        let registry = formats();
        let net = registry
            .iter()
            .find(|f| f.name == "net-settings")
            .expect("registered");
        let old = b"(format_version: 1, replication: (send_every_ticks: 7))";
        let bytes = (net.migrate)(old, 1).expect("network config upgrades");
        let settings =
            regia_net::NetSettings::from_ron(std::str::from_utf8(&bytes).expect("utf8")).expect("load");
        // Version 4 since ADR-10200 (`session.rate_control.first_flight`, off by default,
        // ADR-10231): a v1 file migrates to it with the first flight off, as it ran.
        assert_eq!(settings.format_version, 4);
        assert!(!settings.session.rate_control.first_flight);
        assert_eq!(settings.replication.send_every_ticks, 7);
        assert_eq!(settings.replication.entropy, regia_net::entropy::EntropyMode::Off);
        assert!((net.migrate)(b"\xff", 1).is_err());
        let server = registry
            .iter()
            .find(|f| f.name == "server-toml")
            .expect("registered");
        for version in [1, 2, 3] {
            let text = format!("format_version = {version} # keep\n[levels]\nrotation = [\"arena\"]\n");
            let bytes = (server.migrate)(text.as_bytes(), version).expect("server config upgrades");
            let text = std::str::from_utf8(&bytes).expect("utf8");
            assert!(text.starts_with("format_version = 4 # keep\n"));
            assert_eq!(server_toml_version(&bytes), Ok(Some(4)));
        }
    }

    /// `.netprofile.ron` v1 → v2 (ADR-8522): the version is rewritten, comments and fields stay,
    /// and the result loads with the extrapolation gate off.
    #[test]
    fn netprofile_v1_upgrades_to_v2_with_the_gate_off() {
        let v1 = b"// sniper\nNetProfile(\n    format_version: 1, // pinned\n    priority: 2.0,\n)\n";
        let v2 = migrate_net_profile(v1, 1).expect("migrates");
        let v2 = String::from_utf8(v2).expect("utf-8");
        assert!(v2.starts_with("// sniper\n"), "{v2}");
        assert!(v2.contains("format_version: 2, // pinned"), "{v2}");
        let p = regia_net::profile::NetProfile::from_ron(&v2).expect("loads");
        assert_eq!(p.format_version, 2);
        assert_eq!(p.priority, 2.0);
        assert_eq!(p.extrapolation_gate_m, None);
        assert!(
            migrate_net_profile(b"(priority: 0.0)", 1).is_err(),
            "an invalid profile is not rewritten"
        );
    }

    #[test]
    fn upgrade_project_moves_net_profiles_to_v2() {
        // ADR-8031: v2 adds the optional `rate_tiers`; the step rewrites the version number only.
        let registry = formats();
        let f = registry
            .iter()
            .find(|f| f.name == "netprofile")
            .expect("registered");
        assert_eq!(f.current, 2);
        let old = b"(\n    format_version: 1, // kept\n    priority: 3.0,\n)\n";
        let bytes = (f.migrate)(old, 1).expect("profile upgrades");
        let text = std::str::from_utf8(&bytes).expect("utf8");
        assert_eq!(text, "(\n    format_version: 2, // kept\n    priority: 3.0,\n)\n");
        let p = regia_net::NetProfile::from_ron(text).expect("load");
        assert_eq!((p.format_version, p.priority, p.rate_tiers), (2, 3.0, None));
        assert_eq!(read_ron_version(&bytes), Ok(2));
        // A current file is unchanged; a broken one is refused.
        assert_eq!((f.migrate)(&bytes, 2).expect("current"), bytes);
        assert!((f.migrate)(b"(priority: -1.0)", 1).is_err());
    }

    #[test]
    fn sweep_probes_read_the_versions_they_own() {
        assert_eq!(
            server_toml_version(b"format_version = 1\n[server]\nport = 7777\n"),
            Ok(Some(1))
        );
        assert_eq!(server_toml_version(b"[server]\nport = 7777\n"), Ok(Some(1)));
        // v1 -> v4 (ADR-7950, ADR-8345): the version line is rewritten, comments and keys stay.
        let v1 = b"# mine\nformat_version = 1 # pinned\n[server]\nport = 7777\n[levels]\nrotation = [\"a.level.ron\"]\n";
        let v4 = migrate_server_toml(v1, 1).expect("migrates");
        let v4 = String::from_utf8(v4).expect("utf-8");
        assert!(v4.contains("format_version = 4 # pinned"), "{v4}");
        assert!(v4.starts_with("# mine\n"), "{v4}");
        assert_eq!(server_toml_version(v4.as_bytes()), Ok(Some(4)));
        let bare = migrate_server_toml(b"[server]\nport = 7777\n", 1).expect("migrates");
        assert_eq!(server_toml_version(&bare), Ok(Some(4)));
        assert!(server_toml_version(b"format_version = \"one\"").is_err());
        assert_eq!(
            permutations_version(br#"{"format_version": 1, "platform": "linux"}"#),
            Ok(Some(1))
        );
        assert!(permutations_version(b"[]").is_err());
        let mut registry = regia_asset::registry::REGISTRY_MAGIC.to_vec();
        registry.extend_from_slice(&1u32.to_le_bytes());
        assert_eq!(asset_registry_version(&registry), Ok(Some(1)));
        assert!(asset_registry_version(b"RGASREG").is_err());
        // Postcard writes a u32 as a LEB128 varint: 1 is one byte, 300 two.
        assert_eq!(postcard_version(&[1, 0, 0]), Ok(Some(1)));
        assert_eq!(postcard_leading_u32(&[0xac, 0x02]), Ok(300));
        assert!(postcard_leading_u32(&[0xff, 0xff, 0xff, 0xff, 0xff]).is_err());
        assert!(postcard_leading_u32(&[]).is_err());
        let weights = regia_terrain::LayerWeightMap::new(2, 2, 1)
            .expect("map")
            .to_bytes()
            .expect("bytes");
        assert_eq!(
            postcard_version(&weights),
            Ok(Some(regia_terrain::LAYER_WEIGHTS_FORMAT_VERSION))
        );
        assert_eq!(data_table_version(b"// rows\n[(\"a\", (hp: 1))]"), Ok(Some(0)));
        assert_eq!(
            data_table_version(b"(format_version: 1, row_type: \"x\", rows: {})"),
            Ok(Some(1))
        );
        assert!(data_table_version(b"5").is_err());
    }

    #[test]
    fn session_v1_upgrades_to_v2_keeping_its_text() {
        let v1 = "// smoke\n(format_version: 1, build: Packaged(path: \"game\"), roles: [(name: \"game\", kind: Game)])\n";
        let out = String::from_utf8(migrate_session(v1.as_bytes(), 1).expect("migrates")).expect("utf8");
        assert_eq!(out, v1.replace("format_version: 1", "format_version: 2"));
        assert_eq!(
            regia_cli::session::Session::parse(&out)
                .expect("parses")
                .format_version,
            2
        );
        // Not a valid session: refused, not rewritten.
        assert!(migrate_session(b"(format_version: 1, build: Packaged(path: \"g\"), roles: [])", 1).is_err());
        assert!(migrate_session(b"\xff", 1).is_err());
    }

    #[test]
    fn data_table_v0_upgrades_to_the_v1_envelope() {
        let v0 = b"[(\"goblin\", (hp: 10)), (\"orc\", (hp: 20))]";
        let out = String::from_utf8(migrate_data_table(v0, 0).expect("migrates")).expect("utf8");
        assert_eq!(data_table_version(out.as_bytes()), Ok(Some(1)), "{out}");
        assert!(out.contains("goblin") && out.contains("orc"), "{out}");
        assert!(migrate_data_table(b"not ron", 0).is_err());
    }

    #[test]
    fn input_overrides_v1_upgrades_to_v2() {
        let v1 = b"(contexts: {\"gameplay\": {\"jump\": (action: \"jump\", input: Key(\"KeyJ\"))}})";
        assert_eq!(read_implicit_v1(v1), Ok(1));
        let out = migrate_input_overrides(v1, 1).expect("migrates");
        assert_eq!(read_implicit_v1(&out), Ok(regia_input::OVERRIDES_VERSION));
        let text = std::str::from_utf8(&out).expect("utf8");
        let back = regia_input::InputOverrides::from_ron(text).expect("re-parses");
        assert_eq!(back.contexts["gameplay"]["jump"].action, "jump", "{text}");
        assert!(back.slots.is_empty());
        assert!(migrate_input_overrides(b"(contexts: 5)", 1).is_err());
    }

    #[test]
    fn demo_v1_upgrades_to_v2_with_empty_timestamps() {
        let v1 = "(header:(format_version:1,engine_version:\"0.2.6\",build_hash:\"\",content_hash:\"\",\
                  seed:1,level:\"l\",tick_rate_hz:60,step_count:1,keyframe_interval:0),\
                  actions:[\"Move\"],steps:[[(0.5,0.0,0.0)]],\
                  keyframes:[(tick:0,state_hash:7,snapshot_hex:\"\"),(tick:1,state_hash:8,snapshot_hex:\"\")])\n\
                  // regia-demo-end\n";
        assert_eq!(demo_file_version(v1.as_bytes()), Ok(Some(1)));
        let out = migrate_demo(v1.as_bytes(), 1).expect("migrates");
        assert_eq!(demo_file_version(&out), Ok(Some(2)));
        let demo = regia::framework::demo::DemoRecording::from_ron(std::str::from_utf8(&out).expect("utf8"))
            .expect("re-parses");
        assert_eq!(demo.steps[0][0].x, 0.5);
        assert!(demo.timestamps.is_empty());
        // Not a demo at all: skipped, not an error; a cut-off demo has no end marker either.
        assert_eq!(demo_file_version(b"(version: 1)"), Ok(None));
        assert!(migrate_demo(b"\xff", 1).is_err());
    }

    #[test]
    fn statetree_v1_upgrades_by_rewriting_only_the_version() {
        let v1 = "// guard\n(\n    format_version: 1,\n    root: \"R\",\n    states: [(name: \"R\")],\n)\n";
        let out = migrate_statetree(v1.as_bytes(), 1).expect("migrates");
        let text = String::from_utf8(out).expect("utf8");
        assert_eq!(text, v1.replace("format_version: 1", "format_version: 2"));
        assert!(regia_ai::state_tree::StateTreeDef::from_ron(&text).is_ok());
        // A file already at the current version is returned unchanged; garbage is an error.
        assert_eq!(
            migrate_statetree(text.as_bytes(), 2).expect("unchanged"),
            text.as_bytes()
        );
        assert!(migrate_statetree(b"\xff", 1).is_err());
    }

    #[test]
    fn spatialquery_v1_upgrades_by_rewriting_only_the_version() {
        let v1 = "// flank\n(\n    format_version: 1,\n    name: \"q\",\n    item_type: Point,\n    \
                  generator: AtAnchor(anchor: Asker),\n)\n";
        let out = migrate_spatialquery(v1.as_bytes(), 1).expect("migrates");
        let text = String::from_utf8(out).expect("utf8");
        assert_eq!(text, v1.replace("format_version: 1", "format_version: 2"));
        assert!(regia_ai::spatial_query::SpatialQueryAsset::parse(&text).is_ok());
        assert_eq!(
            migrate_spatialquery(text.as_bytes(), 2).expect("unchanged"),
            text.as_bytes()
        );
        assert!(migrate_spatialquery(b"\xff", 1).is_err());
    }

    #[test]
    fn no_migration_errors_on_an_older_version() {
        let err = no_migration(b"", 0).expect_err("no older versions exist");
        assert!(err.contains("v0"), "{err}");
    }

    /// The `--write` path, exercised through the real [`run_with`] code (not a hand-rolled copy):
    /// a synthetic format with a v0 → v1 step is migrated in place and the original is kept as
    /// `<file>.bak`.
    #[test]
    fn write_migrates_in_place_with_a_backup() {
        let dir = std::env::temp_dir().join(format!("regia-upgrade-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("tmp dir");
        let file = dir.join("thing.custom");
        std::fs::write(&file, "(version: 0)").expect("fixture");

        // A migrator that rewrites `version: 0` to `version: 1` — the step `upgrade-project --write`
        // points at for a future format bump.
        let synthetic = Format {
            name: "custom",
            suffixes: &[".custom"],
            current: 1,
            source: VersionSource::Ron,
            migrate: |bytes, from| {
                assert_eq!(from, 0);
                let text = String::from_utf8(bytes.to_vec()).expect("utf8");
                Ok(text.replace("version: 0", "version: 1").into_bytes())
            },
        };

        let args = UpgradeProjectArgs {
            project: dir.clone(),
            write: true,
            engine: None,
            json: false,
            backup: None,
        };
        run_with(&args, &[synthetic]).expect("--write migrates in place");

        // The file was rewritten in place, and the original was backed up first.
        assert_eq!(std::fs::read_to_string(&file).expect("read"), "(version: 1)");
        assert_eq!(
            std::fs::read_to_string(dir.join("thing.custom.bak")).expect("backup"),
            "(version: 0)"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Cooked voxel far v1 → v2 through the real registry (ADR-2655): a v1 proxy in the dev cache
    /// is rewritten at v2 with a backup and loads to the same proxy; a cooked image is untouched.
    #[test]
    fn upgrade_project_rewrites_v1_voxel_far_proxies() {
        use regia::render::visbuffer::{
            VOXEL_FAR_FORMAT_VERSION, VOXEL_FAR_TYPE_PATH, VoxelBrick, VoxelFarProxy, VoxelLevel,
            VoxelSample, cooked_voxel_far_file_version,
        };
        use regia_asset::{AssetId, AssetTypeTag, COOKED_FORMAT_VERSION, CookedHeader, encode_cooked_file};
        let dir = std::env::temp_dir().join(format!("regia-upgrade-voxel-far-{}", std::process::id()));
        let cache = dir.join(".regia/cooked/linux");
        std::fs::create_dir_all(&cache).expect("tmp dir");
        // One voxel at one level: the v1 blob is the current one at version 1 without the box array
        // (one u32), the sampled box errors (one f32 per level, v3) and the coverage count (one
        // u32, v4); the v3 blob is the current one without the coverage count (ADR-3227).
        let proxy = VoxelFarProxy {
            origin: [0.0; 3],
            extent: 1.0,
            bounds: [0.5, 0.5, 0.5, 0.9],
            source_triangles: 2,
            levels: vec![VoxelLevel {
                resolution: 1,
                voxel_size: 1.0,
                error: 1.732_050_8,
                brick_offset: 0,
                brick_count: 1,
            }],
            bricks: vec![VoxelBrick {
                coord: 0,
                voxel_offset: 0,
                mask_lo: 1,
                mask_hi: 0,
            }],
            voxels: vec![VoxelSample::new([0.0, 1.0, 0.0], [0.5; 3], 1.0, [0.0; 2])],
            extents: Vec::new(),
            sampled_errors: Vec::new(),
            coverage: Vec::new(),
        };
        let mut v1 = proxy.to_bytes();
        v1[4..8].copy_from_slice(&1u32.to_le_bytes());
        v1.truncate(v1.len() - 12);
        let mut v3 = proxy.to_bytes();
        v3[4..8].copy_from_slice(&3u32.to_le_bytes());
        v3.truncate(v3.len() - 4);
        let header = |tag: regia_core::id::TypePathHash, len: usize| CookedHeader {
            format_version: COOKED_FORMAT_VERSION,
            type_tag: tag,
            asset_id: AssetId::new_random(),
            importer_version: 1,
            dependencies: vec![],
            payload_len: len as u64,
        };
        let tag = regia_core::id::TypePathHash::of(VOXEL_FAR_TYPE_PATH);
        let file = encode_cooked_file(&header(tag, v1.len()), &v1).expect("voxel file");
        let image = encode_cooked_file(&header(AssetTypeTag::image().0, 3), b"abc").expect("image file");
        let file3 = encode_cooked_file(&header(tag, v3.len()), &v3).expect("voxel v3 file");
        assert_eq!(cooked_voxel_far_file_version(&file3), Ok(Some(3)));
        std::fs::write(cache.join("voxels.fckd"), &file).expect("write");
        std::fs::write(cache.join("voxels3.fckd"), &file3).expect("write");
        std::fs::write(cache.join("image.fckd"), &image).expect("write");
        let args = UpgradeProjectArgs {
            project: dir.clone(),
            write: true,
            engine: None,
            json: false,
            backup: None,
        };
        run_with(&args, &formats()).expect("--write");
        let migrated = std::fs::read(cache.join("voxels.fckd")).expect("voxels");
        assert_eq!(
            cooked_voxel_far_file_version(&migrated),
            Ok(Some(VOXEL_FAR_FORMAT_VERSION))
        );
        assert_eq!(VoxelFarProxy::from_cooked_file(&migrated).expect("v2"), proxy);
        assert_eq!(
            std::fs::read(cache.join("voxels.fckd.bak")).expect("backup"),
            file
        );
        let migrated3 = std::fs::read(cache.join("voxels3.fckd")).expect("voxels3");
        assert_eq!(
            cooked_voxel_far_file_version(&migrated3),
            Ok(Some(VOXEL_FAR_FORMAT_VERSION))
        );
        assert_eq!(VoxelFarProxy::from_cooked_file(&migrated3).expect("v4"), proxy);
        assert_eq!(std::fs::read(cache.join("image.fckd")).expect("image"), image);
        assert!(!cache.join("image.fckd.bak").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Cooked mesh v1 → v2 through the real registry: a v1 `.fckd` mesh in the dev cache is
    /// rewritten at v2 (with a backup) and loads to the same vertices; a cooked image beside it
    /// is not touched.
    #[test]
    fn write_upgrades_v1_cooked_meshes_and_skips_other_cooked_files() {
        use regia_asset::{
            AssetId, AssetTypeTag, COOKED_FORMAT_VERSION, CookedHeader, CookedIndexWidth, CookedMesh,
            encode_cooked_file, load_mesh_bytes, pack_importer_version,
        };
        let dir = std::env::temp_dir().join(format!("regia-upgrade-mesh-{}", std::process::id()));
        let cache = dir.join(".regia/cooked/linux");
        std::fs::create_dir_all(&cache).expect("tmp dir");
        let mesh = CookedMesh {
            aabb_min: [0.0; 3],
            aabb_max: [1.0, 1.0, 0.0],
            bounding_sphere: [0.5, 0.5, 0.0, 0.8],
            stream_positions: [0.0f32, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0]
                .iter()
                .flat_map(|f| f.to_le_bytes())
                .collect(),
            stream_normals_tangents: vec![0u8; 3 * 8],
            stream_uv0: vec![0u8; 3 * 4],
            indices: [0u16, 1, 2].iter().flat_map(|i| i.to_le_bytes()).collect(),
            index_width: CookedIndexWidth::U16,
            vertex_count: 3,
            lods: vec![],
            submeshes: vec![],
            lod_metric: regia_asset::LOD_METRIC_ERROR,
            format_version: regia_asset::COOKED_MESH_FORMAT_VERSION,
            skin: None,
            stream_color0: Vec::new(),
            morph_targets: Vec::new(),
            skin_influences: regia_asset::DEFAULT_SKIN_INFLUENCES,
        };
        let header = |tag: AssetTypeTag, len: usize| CookedHeader {
            format_version: COOKED_FORMAT_VERSION,
            type_tag: tag.0,
            asset_id: AssetId::new_random(),
            importer_version: pack_importer_version(1, 7, 0),
            dependencies: vec![],
            payload_len: len as u64,
        };
        let v1 = mesh.to_postcard_v1().expect("v1 payload");
        let mesh_file = encode_cooked_file(&header(AssetTypeTag::mesh(), v1.len()), &v1).expect("mesh file");
        let image_file = encode_cooked_file(&header(AssetTypeTag::image(), 3), b"abc").expect("image file");
        std::fs::write(cache.join("mesh.fckd"), &mesh_file).expect("write mesh");
        std::fs::write(cache.join("image.fckd"), &image_file).expect("write image");

        let args = UpgradeProjectArgs {
            project: dir.clone(),
            write: true,
            engine: None,
            json: false,
            backup: None,
        };
        run_with(&args, &formats()).expect("--write");

        let migrated = std::fs::read(cache.join("mesh.fckd")).expect("mesh");
        assert_eq!(
            regia_asset::cooked_mesh_file_version(&migrated).expect("version"),
            Some(regia_asset::COOKED_MESH_FORMAT_VERSION)
        );
        assert_eq!(
            std::fs::read(cache.join("mesh.fckd.bak")).expect("backup"),
            mesh_file
        );
        assert_eq!(
            load_mesh_bytes(&migrated).expect("v2").mesh,
            load_mesh_bytes(&mesh_file).expect("v1").mesh
        );
        assert_eq!(
            std::fs::read(cache.join("image.fckd")).expect("image"),
            image_file
        );
        assert!(!cache.join("image.fckd.bak").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Cooked mesh v2 → v3 (ADR-1622): `--write` rewrites a v2 mesh at v3 with four skin
    /// influences, and it loads to the same vertices.
    #[test]
    fn write_upgrades_v2_cooked_meshes_to_v3() {
        use regia_asset::{
            AssetId, AssetTypeTag, COOKED_FORMAT_VERSION, CookedHeader, CookedIndexWidth, CookedMesh,
            encode_cooked_file, load_mesh_bytes, pack_importer_version,
        };
        let dir = regia_test_utils::unique_temp_dir("upgrade-mesh-v2");
        let cache = dir.join(".regia/cooked/linux");
        std::fs::create_dir_all(&cache).expect("tmp dir");
        let mesh = CookedMesh {
            aabb_min: [0.0; 3],
            aabb_max: [1.0, 1.0, 0.0],
            bounding_sphere: [0.5, 0.5, 0.0, 0.8],
            stream_positions: [0.0f32, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0]
                .iter()
                .flat_map(|f| f.to_le_bytes())
                .collect(),
            stream_normals_tangents: vec![0u8; 3 * 8],
            stream_uv0: vec![0u8; 3 * 4],
            indices: [0u16, 1, 2].iter().flat_map(|i| i.to_le_bytes()).collect(),
            index_width: CookedIndexWidth::U16,
            vertex_count: 3,
            lods: vec![],
            submeshes: vec![],
            lod_metric: regia_asset::LOD_METRIC_ERROR,
            format_version: regia_asset::COOKED_MESH_FORMAT_VERSION,
            skin: None,
            stream_color0: Vec::new(),
            morph_targets: Vec::new(),
            skin_influences: regia_asset::DEFAULT_SKIN_INFLUENCES,
        };
        let v2 = mesh.to_postcard_v2().expect("v2 payload");
        let header = CookedHeader {
            format_version: COOKED_FORMAT_VERSION,
            type_tag: AssetTypeTag::mesh().0,
            asset_id: AssetId::new_random(),
            importer_version: pack_importer_version(1, 7, 0),
            dependencies: vec![],
            payload_len: v2.len() as u64,
        };
        let mesh_file = encode_cooked_file(&header, &v2).expect("mesh file");
        assert_eq!(
            regia_asset::cooked_mesh_file_version(&mesh_file).expect("version"),
            Some(2)
        );
        std::fs::write(cache.join("mesh.fckd"), &mesh_file).expect("write mesh");
        let args = UpgradeProjectArgs {
            project: dir.clone(),
            write: true,
            engine: None,
            json: false,
            backup: None,
        };
        run_with(&args, &formats()).expect("--write");
        let migrated = std::fs::read(cache.join("mesh.fckd")).expect("mesh");
        assert_eq!(
            regia_asset::cooked_mesh_file_version(&migrated).expect("version"),
            Some(3)
        );
        let loaded = load_mesh_bytes(&migrated).expect("v3").mesh;
        assert_eq!(loaded, load_mesh_bytes(&mesh_file).expect("v2").mesh);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `--write` on an older file with no registered step is a clear error, not a silent no-op or
    /// a corrupted rewrite.
    #[test]
    fn write_without_a_migrator_is_an_error() {
        let dir = std::env::temp_dir().join(format!("regia-upgrade-nomig-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("tmp dir");
        std::fs::write(dir.join("thing.custom"), "(version: 0)").expect("fixture");

        let no_step = Format {
            name: "custom",
            suffixes: &[".custom"],
            current: 1,
            source: VersionSource::Ron,
            migrate: no_migration,
        };
        let args = UpgradeProjectArgs {
            project: dir.clone(),
            write: true,
            engine: None,
            json: false,
            backup: None,
        };
        let err = run_with(&args, &[no_step]).expect_err("no migrator must fail");
        assert!(format!("{err:#}").contains("v0"), "{err:#}");
        // The file is untouched.
        assert_eq!(
            std::fs::read_to_string(dir.join("thing.custom")).expect("read"),
            "(version: 0)"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// ADR-8811: a failing file next to a level directory that needs migration leaves the
    /// directory byte-identical and removes its staged copy; once the file is gone the same run
    /// swaps the migrated directory in, keeps the original as `.bak` and leaves no staged copy.
    #[test]
    fn a_failing_file_leaves_a_level_directory_untouched_and_unstaged() {
        let project = regia_test_utils::unique_temp_dir("upgrade_two_phase_dir");
        let levels = project.join("assets/levels");
        let root = levels.join("hall.level.d");
        let mut scene = regia_scene::format::SceneFile::new();
        let guid = regia_core::id::Guid::from_u128(0xef);
        scene.entities.push(regia_scene::format::SceneEntity {
            guid,
            ..Default::default()
        });
        regia_scene::level_dir::write_level_dir(&root, &scene).expect("write dir");
        let entity = root.join(regia_scene::level_dir::entity_rel_path(guid));
        let current = regia_scene::format::SCENE_FORMAT_VERSION;
        let text = std::fs::read_to_string(&entity).expect("read");
        std::fs::write(
            &entity,
            text.replace(
                &format!("scene_format_version: {current}"),
                &format!("scene_format_version: {}", current - 1),
            ),
        )
        .expect("age the file");
        assert!(plan_level_dirs(&project).expect("dirs")[0].needs_migration());
        std::fs::write(project.join("broken.custom"), "(version: 0)").expect("fixture");
        let picky = || Format {
            name: "custom",
            suffixes: &[".custom"],
            current: 1,
            source: VersionSource::Ron,
            migrate: |_, _| Err("this fixture does not migrate".into()),
        };
        let args = UpgradeProjectArgs {
            project: project.clone(),
            write: true,
            engine: None,
            json: false,
            backup: None,
        };
        let snapshot = |dir: &Path| -> Vec<(PathBuf, Vec<u8>)> {
            let mut out = Vec::new();
            let mut stack = vec![dir.to_path_buf()];
            while let Some(d) = stack.pop() {
                for e in std::fs::read_dir(&d).expect("list").filter_map(Result::ok) {
                    if e.path().is_dir() {
                        stack.push(e.path());
                    } else {
                        out.push((e.path(), std::fs::read(e.path()).expect("read")));
                    }
                }
            }
            out.sort();
            out
        };
        let before = snapshot(&project);
        let err = format!("{:#}", run_with(&args, &[picky()]).expect_err("the file fails"));
        assert!(err.contains("broken.custom"), "{err}");
        assert_eq!(
            snapshot(&project),
            before,
            "the project is byte-identical, no staged or .bak file"
        );
        let names = |dir: &Path| -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(dir)
                .expect("list")
                .filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        };
        assert_eq!(names(&levels), ["hall.level.d"], "no staged copy is left");

        std::fs::remove_file(project.join("broken.custom")).expect("drop the bad file");
        run_with(&args, &[picky()]).expect("the directory migrates alone");
        assert_eq!(names(&levels), ["hall.level.d", "hall.level.d.bak"]);
        assert!(!plan_level_dirs(&project).expect("dirs")[0].needs_migration());
        std::fs::remove_dir_all(&project).ok();
    }

    /// A project holding `assets/levels/hall.level.d` (current versions) and the staged copy of it
    /// a killed `--write` left (`.hall.level.d.<pid>.stage/hall.level.d`). Returns (project, stage).
    fn project_with_stale_stage(tag: &str) -> (PathBuf, PathBuf) {
        let project = regia_test_utils::unique_temp_dir(tag);
        let levels = project.join("assets/levels");
        let root = levels.join("hall.level.d");
        let mut scene = regia_scene::format::SceneFile::new();
        scene.entities.push(regia_scene::format::SceneEntity {
            guid: regia_core::id::Guid::from_u128(0xfe),
            ..Default::default()
        });
        regia_scene::level_dir::write_level_dir(&root, &scene).expect("write dir");
        let stage = levels.join(".hall.level.d.999999.stage");
        copy_dir(&root, &stage.join("hall.level.d")).expect("stale staged copy");
        (project, stage)
    }

    /// ADR-8861 (review w7 finding 4): a staged copy an interrupted `--write` left is not a second
    /// level: the upgrade plan sees one level directory and no file inside the staging directory.
    #[test]
    fn a_stale_stage_directory_is_not_a_second_level() {
        let (project, _stage) = project_with_stale_stage("upgrade_stale_stage_plan");
        let dirs = plan_level_dirs(&project).expect("dirs");
        let paths: Vec<String> = dirs.iter().map(|d| distribution::slash_path(&d.path)).collect();
        assert_eq!(paths, ["assets/levels/hall.level.d"], "exactly one level");
        let files = plan_with(&project, &formats()).expect("files");
        assert!(
            files.iter().all(|f| !f.path.to_string_lossy().contains(".stage")),
            "no file of the staged copy is planned: {:?}",
            files
                .iter()
                .map(|f| f.path.display().to_string())
                .collect::<Vec<_>>()
        );
        std::fs::remove_dir_all(&project).ok();
    }

    /// ADR-8861: the dry run says it would remove a stale staging directory and changes nothing;
    /// `--write` removes it with a printed note.
    #[test]
    fn a_stale_stage_directory_is_reported_by_the_dry_run_and_removed_by_write() {
        let (project, stage) = project_with_stale_stage("upgrade_stale_stage_clean");
        let lines = clean_stale_stage_dirs(&project, false);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].contains(".hall.level.d.999999.stage") && lines[0].contains("would remove"),
            "{lines:?}"
        );
        let mut args = UpgradeProjectArgs {
            project: project.clone(),
            write: false,
            engine: None,
            json: false,
            backup: None,
        };
        run_with(&args, &formats()).expect("dry run");
        assert!(stage.join("hall.level.d").is_dir(), "the dry run changes nothing");
        args.write = true;
        run_with(&args, &formats()).expect("write");
        assert!(!stage.exists(), "--write removed the stale staging directory");
        assert!(
            project.join("assets/levels/hall.level.d").is_dir(),
            "the level stays"
        );
        assert!(clean_stale_stage_dirs(&project, true).is_empty());
        std::fs::remove_dir_all(&project).ok();
    }

    /// ADR-8811: `--write` is two-phase. Three old files where the second (and third) migrator
    /// fail: every migration runs first, nothing is renamed, every file stays byte-identical, no
    /// staged or `.bak` file is left behind, and the error names every failing file.
    #[test]
    fn a_failing_migrator_leaves_every_file_untouched() {
        let dir = regia_test_utils::unique_temp_dir("upgrade_two_phase");
        std::fs::create_dir_all(&dir).expect("tmp dir");
        let fixtures = [
            ("a.custom", "(version: 0, ok: true)"),
            ("b.custom", "(version: 0, broken: true)"),
            ("c.custom", "(version: 0, broken: true)"),
        ];
        for (name, text) in fixtures {
            std::fs::write(dir.join(name), text).expect("fixture");
        }
        let picky = Format {
            name: "custom",
            suffixes: &[".custom"],
            current: 1,
            source: VersionSource::Ron,
            migrate: |bytes, _from| {
                let text = String::from_utf8(bytes.to_vec()).map_err(|e| e.to_string())?;
                if text.contains("broken") {
                    return Err("this fixture does not migrate".into());
                }
                Ok(text.replace("version: 0", "version: 1").into_bytes())
            },
        };
        let args = UpgradeProjectArgs {
            project: dir.clone(),
            write: true,
            engine: None,
            json: false,
            backup: None,
        };
        let err = format!(
            "{:#}",
            run_with(&args, &[picky]).expect_err("a failing migrator fails the run")
        );
        assert!(
            err.contains("b.custom") && err.contains("c.custom"),
            "every error is listed: {err}"
        );
        for (name, text) in fixtures {
            assert_eq!(
                std::fs::read_to_string(dir.join(name)).expect("read"),
                text,
                "{name} must be byte-identical after a refused run"
            );
        }
        let mut left: Vec<String> = std::fs::read_dir(&dir)
            .expect("list")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(
            left,
            ["a.custom", "b.custom", "c.custom"],
            "no staged or .bak file is left"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `regia upgrade-project --write` migrates a v1 `.terrain.layers.ron` fixture to the current
    /// [`TERRAIN_LAYERS_FORMAT_VERSION`] (4) through the *real* registry (not a synthetic format),
    /// keeping the original as `<file>.bak`.
    #[test]
    fn write_upgrades_v1_terrain_layers_to_current_version() {
        let dir = std::env::temp_dir().join(format!("regia-upgrade-terrain-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("tmp dir");
        let file = dir.join("ground.terrain.layers.ron");
        let v1 = r#"// `.terrain.layers.ron` fixture (format version 1) — from `docs/formats/terrain.md`.
TerrainLayers(
    version: 1,
    wetness_default: 0.0,
    layers: [
        LayerDesc(
            name: "ground",
            albedo: (0.18, 0.28, 0.10),
            roughness: 0.95,
            tiling: 8.0,
            slope_min: 0.0,
            slope_max: 0.6,
            altitude_min: -1000.0,
            altitude_max: 1000.0,
            blend_sharpness: 0.15,
            triplanar: false,
        ),
    ],
)
"#;
        std::fs::write(&file, v1).expect("fixture");

        let args = UpgradeProjectArgs {
            project: dir.clone(),
            write: true,
            engine: None,
            json: false,
            backup: None,
        };
        run_with(&args, &formats()).expect("--write migrates the v1 terrain layers");

        let migrated = std::fs::read_to_string(&file).expect("read migrated file");
        // The on-disk version must be rewritten to the current format version (peeked raw, so the
        // loader's in-memory migration cannot mask a file that was left at v1).
        assert_eq!(
            read_ron_version(migrated.as_bytes()).expect("migrated file version"),
            regia_terrain::TERRAIN_LAYERS_FORMAT_VERSION,
            "the v1 file must be rewritten at the current format version"
        );
        // And the migrated file still parses as a valid layer stack.
        let layers = regia_terrain::TerrainLayers::from_ron(&migrated).expect("migrated file still parses");
        assert_eq!(layers.version, regia_terrain::TERRAIN_LAYERS_FORMAT_VERSION);

        let backup = std::fs::read_to_string(dir.join("ground.terrain.layers.ron.bak")).expect("backup");
        assert_eq!(
            read_ron_version(backup.as_bytes()).expect("backup version"),
            1,
            "the .bak keeps the v1 original"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `regia upgrade-project --write` migrates a v3 `.terrain.layers.ron` (no weather fields)
    /// to v4 (spec 99 § 7.3): the version moves, the stack holds puddles, no layer gains a
    /// weather block, and every v3 field (the ORM slot, the blend knobs) survives.
    #[test]
    fn write_upgrades_v3_terrain_layers_to_v4_weather() {
        let dir = regia_test_utils::unique_temp_dir("upgrade-terrain-v3");
        let file = dir.join("ground.terrain.layers.ron");
        let v3 = r#"TerrainLayers(
    version: 3,
    wetness_default: 0.1,
    layers: [
        LayerDesc(
            name: "mud",
            albedo: (0.3, 0.25, 0.2),
            roughness: 0.9,
            tiling: 5.0,
            slope_min: 0.0,
            slope_max: 0.6,
            altitude_min: -1000.0,
            altitude_max: 1000.0,
            blend_sharpness: 0.15,
            triplanar: false,
            orm_texture: Some("mud_orm.png"),
            detail_normal_strength: 0.4,
            height_blend: 0.2,
        ),
    ],
)
"#;
        std::fs::write(&file, v3).expect("fixture");
        let args = UpgradeProjectArgs {
            project: dir.clone(),
            write: true,
            engine: None,
            json: false,
            backup: None,
        };
        run_with(&args, &formats()).expect("--write migrates the v3 terrain layers");
        let migrated = std::fs::read_to_string(&file).expect("read migrated file");
        // Through v4 to the current version (v5 since ADR-3367).
        assert_eq!(
            read_ron_version(migrated.as_bytes()).expect("version"),
            regia_terrain::TERRAIN_LAYERS_FORMAT_VERSION
        );
        let layers = regia_terrain::TerrainLayers::from_ron(&migrated).expect("v4 parses");
        assert!(layers.puddles);
        assert!(layers.layers[0].weather.is_none());
        assert_eq!(layers.layers[0].orm_texture.as_deref(), Some("mud_orm.png"));
        assert_eq!(layers.layers[0].height_blend, 0.2);
        assert_eq!(layers.wetness_default, 0.1);
        let backup = std::fs::read_to_string(dir.join("ground.terrain.layers.ron.bak")).expect("backup");
        assert_eq!(read_ron_version(backup.as_bytes()).expect("backup version"), 3);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `regia upgrade-project --write` migrates a v4 `.terrain.layers.ron` (an ORM layer with a
    /// weather block) to v5 (ADR-3367): the version moves, the ORM map's blue channel stays unread
    /// (`porosity_from_orm` false), and the v4 weather block survives.
    #[test]
    fn write_upgrades_v4_terrain_layers_to_v5_without_reading_orm_blue() {
        let dir = regia_test_utils::unique_temp_dir("upgrade-terrain-v4");
        let file = dir.join("ground.terrain.layers.ron");
        let v4 = r#"TerrainLayers(
    version: 4,
    wetness_default: 0.0,
    layers: [
        LayerDesc(
            name: "mud",
            albedo: (0.3, 0.25, 0.2),
            roughness: 0.9,
            tiling: 5.0,
            slope_min: 0.0,
            slope_max: 0.6,
            altitude_min: -1000.0,
            altitude_max: 1000.0,
            blend_sharpness: 0.15,
            triplanar: false,
            orm_texture: Some("mud_orm.png"),
            weather: Some((porosity: Some(0.8), snow: 0.5)),
        ),
    ],
    puddles: false,
)
"#;
        std::fs::write(&file, v4).expect("fixture");
        let args = UpgradeProjectArgs {
            project: dir.clone(),
            write: true,
            engine: None,
            json: false,
            backup: None,
        };
        run_with(&args, &formats()).expect("--write migrates the v4 terrain layers");
        let migrated = std::fs::read_to_string(&file).expect("read migrated file");
        assert_eq!(read_ron_version(migrated.as_bytes()).expect("version"), 5);
        let layers = regia_terrain::TerrainLayers::from_ron(&migrated).expect("v5 parses");
        assert!(!layers.puddles);
        let weather = layers.layers[0].weather.expect("the block survives");
        assert_eq!((weather.porosity, weather.snow), (Some(0.8), 0.5));
        assert!(
            !weather.porosity_from_orm,
            "a v4 ORM map's blue is not reinterpreted"
        );
        let backup = std::fs::read_to_string(dir.join("ground.terrain.layers.ron.bak")).expect("backup");
        assert_eq!(read_ron_version(backup.as_bytes()).expect("backup version"), 4);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A `~` suffix ignores ASCII case and nothing else (ADR-7978).
    #[test]
    fn any_case_suffixes_ignore_only_ascii_case() {
        assert!(owns_name("~terrain.layers.ron", "terrain.layers.ron"));
        assert!(owns_name("~terrain.layers.ron", "Ground.Terrain.Layers.RON"));
        assert!(!owns_name("~terrain.layers.ron", "terrain.layers.ron.bak"));
        assert!(!owns_name("~terrain.layers.ron", "errain.layers.ron"));
        assert!(!owns_name("~terrain.layers.ron", "forest.layers.ron"));
        // The plain and whole-name forms stay case-sensitive.
        assert!(!owns_name(".bt.ron", "guard.BT.ron"));
        assert!(!owns_name("/net.ron", "Net.ron"));
    }

    /// The terrain-layers row owns exactly the file names the cook (`discover_terrains`) and the live
    /// loader (`SourceKind::TerrainLayers`) take as a terrain layer file, a map's plain
    /// `terrain.layers.ron` included (DO27 CONTENT_050, ADR-7978), and no other row claims them.
    #[test]
    fn terrain_layers_row_owns_the_names_the_cook_and_loader_accept() {
        let registry = formats();
        let row = registry
            .iter()
            .find(|f| f.name == "terrain-layers")
            .expect("registered");
        for name in [
            "terrain.layers.ron",
            "Terrain.Layers.ron",
            "ground.terrain.layers.ron",
            "GROUND.TERRAIN.LAYERS.RON",
            "groundterrain.layers.ron",
            "terrain.layers.ron.bak",
            "terrain.layers.ron~",
            "forest.layers.ron",
            "terrain.layer.ron",
            "layers.ron",
        ] {
            let path = Path::new("maps/forest/textures").join(name);
            let cook = !regia_asset_pipeline::discover_terrains(std::slice::from_ref(&path)).is_empty();
            let loader = regia_asset::SourceKind::of(&path) == regia_asset::SourceKind::TerrainLayers;
            let upgrade = row.suffixes.iter().any(|s| owns_name(s, name));
            assert_eq!(cook, loader, "{name}: the cook and the live loader disagree");
            assert_eq!(upgrade, cook, "{name}: upgrade-project disagrees with the cook");
            if upgrade {
                let others: Vec<&str> = registry
                    .iter()
                    .filter(|f| f.name != "terrain-layers" && f.suffixes.iter().any(|s| owns_name(s, name)))
                    .map(|f| f.name)
                    .collect();
                assert!(others.is_empty(), "{name}: also claimed by {others:?}");
            }
        }
    }

    /// `regia upgrade-project --write` migrates a v3 layer stack stored under DO27's per-map name
    /// `maps/<map>/textures/terrain.layers.ron` and a mixed-case `Cliff.Terrain.Layers.ron`, beside a
    /// `ground.terrain.layers.ron`: every one reaches the current version with a `.bak` of the v3
    /// bytes (DO27 CONTENT_050, ADR-7978).
    #[test]
    fn write_upgrades_terrain_layers_under_the_plain_and_any_case_names() {
        let dir = regia_test_utils::unique_temp_dir("upgrade-terrain-plain-name");
        let v3 = r#"TerrainLayers(
    version: 3,
    wetness_default: 0.1,
    layers: [
        LayerDesc(
            name: "mud",
            albedo: (0.3, 0.25, 0.2),
            roughness: 0.9,
            tiling: 5.0,
            slope_min: 0.0,
            slope_max: 0.6,
            altitude_min: -1000.0,
            altitude_max: 1000.0,
            blend_sharpness: 0.15,
            triplanar: false,
            orm_texture: Some("mud_orm.png"),
            detail_normal_strength: 0.4,
            height_blend: 0.2,
        ),
    ],
)
"#;
        let files = [
            dir.join("maps/forest/textures/terrain.layers.ron"),
            dir.join("maps/quarry/Cliff.Terrain.Layers.ron"),
            dir.join("ground.terrain.layers.ron"),
        ];
        for file in &files {
            std::fs::create_dir_all(file.parent().expect("a parent")).expect("fixture folder");
            std::fs::write(file, v3).expect("fixture");
        }
        let plan = plan_with(&dir, &formats()).expect("plan");
        for file in &files {
            let entry = plan
                .iter()
                .find(|p| dir.join(&p.path) == *file)
                .unwrap_or_else(|| panic!("{} is not planned", file.display()));
            assert_eq!(entry.format, "terrain-layers", "{}", file.display());
        }
        let args = UpgradeProjectArgs {
            project: dir.clone(),
            write: true,
            engine: None,
            json: false,
            backup: None,
        };
        run_with(&args, &formats()).expect("--write migrates the v3 terrain layers");
        for file in &files {
            let migrated = std::fs::read_to_string(file).expect("read migrated file");
            assert_eq!(
                read_ron_version(migrated.as_bytes()).expect("version"),
                regia_terrain::TERRAIN_LAYERS_FORMAT_VERSION,
                "{}",
                file.display()
            );
            let layers = regia_terrain::TerrainLayers::from_ron(&migrated).expect("re-parses");
            assert_eq!(layers.layers[0].orm_texture.as_deref(), Some("mud_orm.png"));
            let mut bak = file.clone().into_os_string();
            bak.push(".bak");
            assert_eq!(
                std::fs::read_to_string(&bak).expect("backup"),
                v3,
                "{}",
                file.display()
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The registry covers the audit-3 gaps: `.vfx.ron` at the crate's current version (not v1),
    /// `.glasscrack.ron` and `.loopgen.ron` present.
    #[test]
    fn registry_lists_vfx_at_current_glasscrack_and_loopgen() {
        let all = formats();
        let find = |name: &str| all.iter().find(|f| f.name == name).expect("registered");
        assert_eq!(find("vfx").current, regia_vfx::VFX_FORMAT_VERSION);
        assert!(find("glasscrack").suffixes.contains(&".glasscrack.ron"));
        assert!(find("loopgen").suffixes.contains(&".loopgen.ron"));
    }

    /// A v6 `.vfx.ron` is re-saved at the current version with no kill volume and no ribbon
    /// (ADR-2800), and loads to the same definition as the file it came from.
    #[test]
    fn write_upgrades_v6_vfx_without_changing_it() {
        let dir = regia_test_utils::unique_temp_dir("upgrade-vfx-v6");
        let v6 = r#"(version: 6, name: "x", emitters: [(
    name: "e", spawn: (mode: Rate(1.0), lifetime: (min: 0.1, max: 0.5)),
    init: (size: (min: 1.0, max: 1.0)), update: (drag: 0.5, wind_response: 0.5, kill_below: Some(0.0)),
    render: (mode: Billboard),
)])"#;
        let old = dir.join("old.vfx.ron");
        std::fs::write(&old, v6).expect("fixture");
        let args = UpgradeProjectArgs {
            project: dir.clone(),
            write: true,
            engine: None,
            json: false,
            backup: None,
        };
        run_with(&args, &formats()).expect("--write migrates the v6 vfx");
        let migrated = std::fs::read_to_string(&old).expect("read migrated");
        assert_eq!(
            read_ron_version(migrated.as_bytes()).expect("version"),
            regia_vfx::VFX_FORMAT_VERSION
        );
        let saved = regia_vfx::VfxSystemDef::from_ron(&migrated).expect("loads");
        assert_eq!(
            saved,
            regia_vfx::VfxSystemDef::from_ron(v6).expect("the original loads")
        );
        assert!(saved.emitters[0].update.kill_volume.is_none());
        assert_eq!(
            saved.emitters[0].render.mode,
            regia_vfx::format::RenderMode::Billboard
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A v7 `.vfx.ron` mesh particle is re-saved at v8 with no `render.mesh` (it keeps the built-in
    /// cube, ADR-7210) and loads to the same definition as the file it came from.
    #[test]
    fn write_upgrades_v7_vfx_mesh_particles_to_v8_keeping_the_cube() {
        let dir = regia_test_utils::unique_temp_dir("upgrade-vfx-v7");
        let v7 = r#"(version: 7, name: "x", emitters: [(
    name: "e", spawn: (mode: Burst(4), lifetime: (min: 1.0, max: 1.0)),
    init: (size: (min: 0.2, max: 0.2)),
    render: (mode: Mesh),
)])"#;
        let old = dir.join("old.vfx.ron");
        std::fs::write(&old, v7).expect("fixture");
        let args = UpgradeProjectArgs {
            project: dir.clone(),
            write: true,
            engine: None,
            json: false,
            backup: None,
        };
        run_with(&args, &formats()).expect("--write migrates the v7 vfx");
        let migrated = std::fs::read_to_string(&old).expect("read migrated");
        assert_eq!(read_ron_version(migrated.as_bytes()).expect("version"), 8);
        assert!(
            !migrated.contains("mesh:"),
            "the cube needs no mesh field: {migrated}"
        );
        let saved = regia_vfx::VfxSystemDef::from_ron(&migrated).expect("loads");
        assert_eq!(
            saved,
            regia_vfx::VfxSystemDef::from_ron(v7).expect("the original loads")
        );
        assert_eq!(saved.emitters[0].render.mesh, None);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Product viewer contract files (ADR-2480, ADR-2535) are found by their `schema` field whatever
    /// their name: a current one is up to date, a newer major is reported as newer than the engine,
    /// and other JSON files are not claimed.
    #[test]
    fn plans_jewelry_contract_files_by_their_schema_field() {
        let dir = regia_test_utils::unique_temp_dir("upgrade-jewelry");
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../regia_configurator/tests/fixtures");
        let build = std::fs::read(fixtures.join("jewelry-build/build-house-a.json")).expect("fixture");
        std::fs::write(dir.join("a.json"), &build).expect("write");
        std::fs::write(dir.join("kit.json"), br#"{"schema": "jewelry.part_kit/2"}"#).expect("write");
        std::fs::write(
            dir.join("other.json"),
            br#"{"schema": "jewelry.option_graph/1", "x": 1}"#,
        )
        .expect("write");
        std::fs::write(dir.join("plain.json"), b"[1, 2, 3]").expect("write");
        let plan = plan_with(&dir, &formats()).expect("plan");
        let found: Vec<(String, &str, u32, u32)> = plan
            .iter()
            .map(|f| (f.path.display().to_string(), f.format, f.found, f.current))
            .collect();
        assert_eq!(
            found,
            vec![
                ("a.json".to_string(), "jewelry-build", 1, 1),
                ("kit.json".to_string(), "jewelry-part-kit", 2, 1),
            ]
        );
        assert!(!plan[0].needs_migration());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A v5 `.vfx.ron` is re-saved at the current version with no wind response and no events
    /// (ADR-2642), and loads to the same definition as the file it came from.
    #[test]
    fn write_upgrades_v5_vfx_without_changing_it() {
        let dir = regia_test_utils::unique_temp_dir("upgrade-vfx-v5");
        let v5 = r#"(version: 5, name: "x", emitters: [(
    name: "e", spawn: (mode: Rate(1.0), lifetime: (min: 0.1, max: 0.5)),
    init: (size: (min: 1.0, max: 1.0)), update: (drag: 0.5), render: (mode: Billboard),
)])"#;
        let old = dir.join("old.vfx.ron");
        std::fs::write(&old, v5).expect("fixture");
        let args = UpgradeProjectArgs {
            project: dir.clone(),
            write: true,
            engine: None,
            json: false,
            backup: None,
        };
        run_with(&args, &formats()).expect("--write migrates the v5 vfx");
        let migrated = std::fs::read_to_string(&old).expect("read migrated");
        assert_eq!(
            read_ron_version(migrated.as_bytes()).expect("version"),
            regia_vfx::VFX_FORMAT_VERSION
        );
        let saved = regia_vfx::VfxSystemDef::from_ron(&migrated).expect("loads");
        assert_eq!(
            saved,
            regia_vfx::VfxSystemDef::from_ron(v5).expect("the original loads")
        );
        assert_eq!(saved.emitters[0].update.wind_response, 0.0);
        assert!(saved.emitters[0].update.events.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A v1 `.vfx.ron` is migrated by `--write` to the current version and a current (v4) file is
    /// reported up to date rather than "newer than engine".
    #[test]
    fn write_upgrades_v1_vfx_and_accepts_current() {
        let dir = regia_test_utils::unique_temp_dir("upgrade-vfx");
        let v1 = r#"(version: 1, name: "x", emitters: [(
    name: "e", spawn: (mode: Rate(1.0), lifetime: (min: 0.1, max: 0.5)),
    init: (size: (min: 1.0, max: 1.0)), render: (mode: Billboard),
)])"#;
        let old = dir.join("old.vfx.ron");
        std::fs::write(&old, v1).expect("fixture");
        let args = UpgradeProjectArgs {
            project: dir.clone(),
            write: true,
            engine: None,
            json: false,
            backup: None,
        };
        run_with(&args, &formats()).expect("--write migrates the v1 vfx");
        let migrated = std::fs::read_to_string(&old).expect("read migrated");
        assert_eq!(
            read_ron_version(migrated.as_bytes()).expect("version"),
            regia_vfx::VFX_FORMAT_VERSION
        );
        let plan = plan_with(&dir, &formats()).expect("plan");
        let vfx: Vec<_> = plan.iter().filter(|f| f.format == "vfx").collect();
        assert_eq!(vfx.len(), 1);
        assert!(!vfx[0].needs_migration());
        assert_eq!(vfx[0].found, vfx[0].current);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `regia upgrade-project --write` migrates a v1 `.scatter.ron` (one rule) to a v2 set with
    /// one class carrying the rule's mesh, keeping the v1 original as `.bak`.
    #[test]
    fn write_upgrades_v1_scatter_to_a_v2_set() {
        let dir = std::env::temp_dir().join(format!("regia-upgrade-scatter-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("tmp dir");
        let file = dir.join("rocks.scatter.ron");
        let v1 = regia_terrain::ScatterRule::default().to_ron().expect("v1 text");
        std::fs::write(&file, &v1).expect("fixture");
        let args = UpgradeProjectArgs {
            project: dir.clone(),
            write: true,
            engine: None,
            json: false,
            backup: None,
        };
        run_with(&args, &formats()).expect("--write migrates the v1 scatter rule");
        let migrated = std::fs::read_to_string(&file).expect("read migrated file");
        assert_eq!(
            read_ron_version(migrated.as_bytes()).expect("migrated version"),
            regia_terrain::SCATTER_FORMAT_VERSION
        );
        let set = regia_terrain::ScatterSet::from_ron(&migrated, None).expect("v2 parses");
        assert_eq!(
            set.classes[0].meshes[0].mesh,
            regia_terrain::ScatterRule::default().mesh
        );
        let backup = std::fs::read_to_string(dir.join("rocks.scatter.ron.bak")).expect("backup");
        assert_eq!(read_ron_version(backup.as_bytes()).expect("backup version"), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `regia upgrade-project --write` rewrites a v1 `.level.ron` at the current scene version
    /// with the atmosphere spelled out (Earth), keeping the v1 original as `.bak`.
    #[test]
    fn write_upgrades_v1_level_to_an_earth_atmosphere() {
        let dir = std::env::temp_dir().join(format!("regia-upgrade-level-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("tmp dir");
        let file = dir.join("mars.level.ron");
        let v1 = r#"SceneFile(
    format_version: 1,
    entities: [],
    resources: {
        "regia_scene::level::LevelSettings": (gravity: (0.0, -3.71, 0.0)),
    },
)
"#;
        std::fs::write(&file, v1).expect("fixture");
        let args = UpgradeProjectArgs {
            project: dir.clone(),
            write: true,
            engine: None,
            json: false,
            backup: None,
        };
        run_with(&args, &formats()).expect("--write migrates the v1 level");
        let migrated = std::fs::read_to_string(&file).expect("read migrated file");
        assert_eq!(
            read_ron_version(migrated.as_bytes()).expect("version"),
            regia_scene::format::SCENE_FORMAT_VERSION
        );
        assert!(migrated.contains("atmosphere"), "{migrated}");
        assert!(migrated.contains("Earth"), "{migrated}");
        let backup = std::fs::read_to_string(dir.join("mars.level.ron.bak")).expect("backup");
        assert_eq!(read_ron_version(backup.as_bytes()).expect("backup version"), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `regia upgrade-project --write` takes a v10 level through v11 (ADR-2313) to the current
    /// version: a level naming its lightmap and saving a receiver keeps both, and its lightmap reads
    /// back.
    #[test]
    fn write_upgrades_a_v10_level_with_a_lightmap_and_receiver_through_v11() {
        let dir = regia_test_utils::unique_temp_dir("regia-upgrade-lightmap-level");
        let file = dir.join("yard.level.ron");
        let v10 = r#"SceneFile(
    format_version: 10,
    entities: [
        (
            guid: "00000000-0000-0000-0000-0000000000a1",
            name: "hall",
            components: {
                "regia_render::lightmap::LightmapReceiver": (entry: "entity_00000000-0000-0000-0000-0000000000a1_0"),
            },
        ),
    ],
    resources: {
        "regia_scene::level::LevelSettings": (look: (lightmap: Some((file: "maps/yard.flmap")))),
    },
)
"#;
        std::fs::write(&file, v10).expect("fixture");
        let args = UpgradeProjectArgs {
            project: dir.clone(),
            write: true,
            engine: None,
            json: false,
            backup: None,
        };
        run_with(&args, &formats()).expect("--write migrates the v10 level");
        let migrated = std::fs::read_to_string(&file).expect("read migrated file");
        assert_eq!(
            read_ron_version(migrated.as_bytes()).expect("version"),
            regia_scene::format::SCENE_FORMAT_VERSION
        );
        assert!(
            migrated.contains("regia_render::lightmap::LightmapReceiver"),
            "{migrated}"
        );
        let level =
            regia_scene::level::Level::from_ron_str(regia_scene::level::LevelId::default(), &migrated)
                .expect("reads back");
        assert_eq!(
            level.settings.look.lightmap.map(|l| l.file),
            Some("maps/yard.flmap".to_string())
        );
        let backup = std::fs::read_to_string(dir.join("yard.level.ron.bak")).expect("backup");
        assert_eq!(read_ron_version(backup.as_bytes()).expect("backup version"), 10);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A profile capture without `format_version` is planned as v0 and re-written at v1 with its
    /// tree kept (ADR-2323).
    #[test]
    fn profile_capture_v0_upgrades_to_current() {
        let formats = formats();
        let capture = formats
            .iter()
            .find(|f| f.name == "profile-capture")
            .expect("profile-capture");
        let v0 = br#"{ "frames": 4, "frame_ms": 2.5, "cpu": [{ "name": "frame", "inclusive_ms": 2.0, "self_ms": 2.0, "calls": 1.0 }] }"#;
        assert_eq!(read_version(capture, v0).expect("version"), Some(0));
        let out = (capture.migrate)(v0, 0).expect("migrate");
        assert_eq!(
            read_version(capture, &out).expect("version"),
            Some(regia_telemetry::profile_capture::PROFILE_CAPTURE_FORMAT_VERSION)
        );
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("\"frame\""), "{text}");
        let newer = br#"{ "format_version": 9, "frames": 1, "frame_ms": 1.0, "cpu": [] }"#;
        assert!((capture.migrate)(newer, 9).is_err(), "a newer capture is refused");
    }

    /// ADR-7325: a v1 profile capture is planned as v1 and re-written at v2 with no timeline and
    /// its tree kept.
    #[test]
    fn profile_capture_v1_upgrades_to_v2() {
        let formats = formats();
        let capture = formats
            .iter()
            .find(|f| f.name == "profile-capture")
            .expect("profile-capture");
        let v1 = br#"{ "format_version": 1, "frames": 4, "frame_ms": 2.5, "cpu": [{ "name": "frame", "inclusive_ms": 2.0, "self_ms": 2.0, "calls": 1.0 }] }"#;
        assert_eq!(read_version(capture, v1).expect("version"), Some(1));
        let out = (capture.migrate)(v1, 1).expect("migrate");
        assert_eq!(read_version(capture, &out).expect("version"), Some(2));
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains("\"frame\"") && text.contains("\"timeline\": []"),
            "{text}"
        );
    }

    /// `regia upgrade-project` plans the post volumes and lenses inside a scene file next to the
    /// file itself, and `--write` migrates an old level and then its components in one run,
    /// keeping one backup with the original bytes (ADR-2323).
    #[test]
    fn write_upgrades_post_volumes_and_lenses_inside_a_level() {
        use super::super::upgrade_components::{PHYSICAL_CAMERA_TYPE_PATH, POST_VOLUME_TYPE_PATH};
        let dir = regia_test_utils::unique_temp_dir("regia-upgrade-embedded");
        let file = dir.join("hall.level.ron");
        let v1 = format!(
            r#"SceneFile(
    format_version: 1,
    entities: [
        (
            guid: "3f7a1c2e-8b64-4a35-9d20-6f1e2b3c4d5a",
            components: {{
                "{POST_VOLUME_TYPE_PATH}": (format_version: 1, shape: Box(half_extents: (2.0, 3.0, 4.0)), priority: 2),
                "{PHYSICAL_CAMERA_TYPE_PATH}": (shutter_angle: 0.25),
            }},
        ),
    ],
)
"#
        );
        std::fs::write(&file, &v1).expect("fixture");
        let mut files = plan_with(&dir, &formats()).expect("plan");
        plan_embedded(&dir, &mut files);
        let listed: Vec<(&str, u32)> = files.iter().map(|f| (f.format, f.found)).collect();
        assert_eq!(
            listed,
            vec![("scene-level", 1), ("post-volume", 1), ("physical-camera", 0)]
        );
        let args = UpgradeProjectArgs {
            project: dir.clone(),
            write: true,
            engine: None,
            json: false,
            backup: None,
        };
        run_with(&args, &formats()).expect("--write migrates the level and its components");
        let mut after = plan_with(&dir, &formats()).expect("plan");
        plan_embedded(&dir, &mut after);
        assert_eq!(after.len(), 3, "the backup is not a scene file");
        assert!(after.iter().all(|f| !f.needs_migration()), "everything current");
        let backup = std::fs::read_to_string(dir.join("hall.level.ron.bak")).expect("backup");
        assert_eq!(backup, v1, "the backup holds the original bytes");
        assert!(!dir.join("hall.level.ron.bak.2").exists(), "one backup per file");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Spec 83 § 3.3: a `.prefab.ron` gets its embedded post volumes and lenses planned and
    /// migrated like a level, with one backup of the original bytes.
    #[test]
    fn write_upgrades_post_volumes_and_lenses_inside_a_prefab() {
        use super::super::upgrade_components::{PHYSICAL_CAMERA_TYPE_PATH, POST_VOLUME_TYPE_PATH};
        let dir = regia_test_utils::unique_temp_dir("regia-upgrade-embedded-prefab");
        let file = dir.join("lamp.prefab.ron");
        let v1 = format!(
            r#"SceneFile(
    format_version: 1,
    entities: [
        (
            guid: "5b1d0c9e-2a47-4f61-8c33-7e0a1b2c3d4e",
            components: {{
                "{POST_VOLUME_TYPE_PATH}": (format_version: 2, shape: Sphere(radius: 3.0), priority: 1),
                "{PHYSICAL_CAMERA_TYPE_PATH}": (shutter_angle: 0.5),
            }},
        ),
    ],
)
"#
        );
        std::fs::write(&file, &v1).expect("fixture");
        let mut files = plan_with(&dir, &formats()).expect("plan");
        plan_embedded(&dir, &mut files);
        let listed: Vec<(&str, u32)> = files.iter().map(|f| (f.format, f.found)).collect();
        assert_eq!(
            listed,
            vec![("scene-prefab", 1), ("post-volume", 2), ("physical-camera", 0)]
        );
        let args = UpgradeProjectArgs {
            project: dir.clone(),
            write: true,
            engine: None,
            json: false,
            backup: None,
        };
        run_with(&args, &formats()).expect("--write migrates the prefab and its components");
        let mut after = plan_with(&dir, &formats()).expect("plan");
        plan_embedded(&dir, &mut after);
        assert_eq!(after.len(), 3, "the backup is not a scene file");
        assert!(after.iter().all(|f| !f.needs_migration()), "everything current");
        let backup = std::fs::read_to_string(dir.join("lamp.prefab.ron.bak")).expect("backup");
        assert_eq!(backup, v1, "the backup holds the original bytes");
        assert!(!dir.join("lamp.prefab.ron.bak.2").exists(), "one backup per file");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `regia upgrade-project --write` moves a v2 `.bake.ron`'s one `sun` into a one-element `suns`
    /// list at the current version (ADR-7818), keeps the comments and the rest of the bytes and the
    /// original as `.bake.ron.bak`; the result loads with the same sun, and a second run changes
    /// nothing.
    #[test]
    fn write_upgrades_a_v2_bake_sun_to_a_suns_list() {
        let current = regia_pt::bake_desc::BAKE_DESC_FORMAT_VERSION;
        let dir = regia_test_utils::unique_temp_dir("regia-upgrade-bake-suns");
        let file = dir.join("yard.bake.ron");
        let v2 = "// Yard\n(\n    format_version: 2,\n    // noon\n    sun: Some((toward: (0.3, 0.8, 0.2), illuminance: 1000.0)),\n    sky: Some((1.0, 1.0, 1.0)),\n    layout: (bricks: [(min: (0.0, 0.0, 0.0), max: (1.0, 1.0, 1.0), spacing: 1.0)]),\n)\n";
        std::fs::write(&file, v2).expect("fixture");
        let args = UpgradeProjectArgs {
            project: dir.clone(),
            write: true,
            engine: None,
            json: false,
            backup: None,
        };
        run_with(&args, &formats()).expect("--write migrates the v2 description");
        let migrated = std::fs::read_to_string(&file).expect("read migrated file");
        assert_eq!(
            migrated,
            v2.replace("format_version: 2,", &format!("format_version: {current},"))
                .replace(
                    "sun: Some((toward: (0.3, 0.8, 0.2), illuminance: 1000.0)),",
                    "suns: [(toward: (0.3, 0.8, 0.2), illuminance: 1000.0)],"
                )
        );
        let desc = regia_pt::bake_desc::GiBakeDescription::from_ron(&migrated).expect("current parses");
        assert_eq!(desc.suns.len(), 1);
        assert_eq!(desc.suns[0].illuminance, 1000.0);
        assert_eq!(
            std::fs::read_to_string(dir.join("yard.bake.ron.bak")).expect("backup"),
            v2
        );
        run_with(&args, &formats()).expect("a second run");
        assert_eq!(
            std::fs::read_to_string(&file).expect("read again"),
            migrated,
            "a second run changes nothing"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `regia upgrade-project --write` stamps a v0 `.bake.ron` (no `format_version`) at the
    /// current version and rewrites a v1 file's version number, keeping their comments and the
    /// originals as `.bake.ron.bak`; the results still parse.
    #[test]
    fn write_upgrades_v0_and_v1_bake_descriptions() {
        let current = regia_pt::bake_desc::BAKE_DESC_FORMAT_VERSION;
        let dir = regia_test_utils::unique_temp_dir("regia-upgrade-bake");
        let file = dir.join("bore.bake.ron");
        let v0 = "// Bore Twelve\n(sky: Some((1.0, 1.0, 1.0)), layout: (bricks: [(min: (0.0, 0.0, 0.0), max: (1.0, 1.0, 1.0), spacing: 1.0)]))\n";
        std::fs::write(&file, v0).expect("fixture");
        let v1_file = dir.join("hab.bake.ron");
        let v1 = "// Hab\n(format_version: 1, sky: Some((1.0, 1.0, 1.0)), layout: (bricks: [(min: (0.0, 0.0, 0.0), max: (1.0, 1.0, 1.0), spacing: 1.0)]))\n";
        std::fs::write(&v1_file, v1).expect("fixture");
        let args = UpgradeProjectArgs {
            project: dir.clone(),
            write: true,
            engine: None,
            json: false,
            backup: None,
        };
        run_with(&args, &formats()).expect("--write migrates the v0 description");
        let migrated = std::fs::read_to_string(&file).expect("read migrated file");
        assert!(
            migrated.starts_with(&format!("// Bore Twelve\n(\n    format_version: {current},")),
            "{migrated}"
        );
        assert_eq!(
            read_bake_version(migrated.as_bytes()).expect("version"),
            Some(current)
        );
        regia_pt::bake_desc::GiBakeDescription::from_ron(&migrated).expect("current parses");
        let backup = std::fs::read_to_string(dir.join("bore.bake.ron.bak")).expect("backup");
        assert_eq!(backup, v0);
        let migrated = std::fs::read_to_string(&v1_file).expect("read migrated v1 file");
        assert_eq!(
            migrated,
            v1.replace("format_version: 1,", &format!("format_version: {current},"))
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("hab.bake.ron.bak")).expect("backup"),
            v1
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_upgrades_a_v1_cooked_clip_and_leaves_other_cooked_files_alone() {
        use regia_animation::AnimationClip;
        use regia_asset::{
            AssetId, COOKED_FORMAT_VERSION, CookedHeader, encode_cooked_file, pack_importer_version,
        };

        let dir = std::env::temp_dir().join(format!("regia-upgrade-clip-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("tmp dir");
        // A clip without morph tracks has the same body in every version; the v1 payload has no
        // magic. The current (v3, ADR-7227) payload is the magic, the version word, the body's
        // `Raw` tag byte, then that same postcard clip.
        let clip = AnimationClip::empty("idle", 1.0);
        let current = clip.to_cooked_payload().expect("payload");
        let v1 = &current[9..];
        let header = |tag| CookedHeader {
            format_version: COOKED_FORMAT_VERSION,
            type_tag: tag,
            asset_id: AssetId::new_random(),
            importer_version: pack_importer_version(1, 1, 0),
            dependencies: vec![],
            payload_len: v1.len() as u64,
        };
        let clip_file = dir.join("idle.fckd");
        std::fs::write(
            &clip_file,
            encode_cooked_file(&header(regia_animation::cooked::clip_type_tag()), v1).expect("encode"),
        )
        .expect("write clip");
        let other_file = dir.join("skeleton.fckd");
        let other =
            encode_cooked_file(&header(regia_animation::cooked::skeleton_type_tag()), v1).expect("encode");
        std::fs::write(&other_file, &other).expect("write other");

        let plan = plan_with(&dir, &formats()).expect("plan");
        assert_eq!(plan.len(), 1, "only the clip is a frozen format file");
        assert_eq!(plan[0].format, "cooked-clip");
        assert!(plan[0].needs_migration());

        let args = UpgradeProjectArgs {
            project: dir.clone(),
            write: true,
            engine: None,
            json: false,
            backup: None,
        };
        run_with(&args, &formats()).expect("--write migrates the v1 clip");
        let migrated = std::fs::read(&clip_file).expect("read migrated clip");
        assert_eq!(
            regia_animation::cooked::cooked_clip_file_version(&migrated).expect("version"),
            Some(regia_animation::clip::CLIP_FORMAT_VERSION)
        );
        assert_eq!(std::fs::read(&other_file).expect("other"), other);
        assert!(dir.join("idle.fckd.bak").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// ADR-7817: a v2 cooked motion database (no clip-speed or arc-length arrays) is rewritten at
    /// the current payload version with its header kept; the database it loads as searches the same.
    #[test]
    fn write_upgrades_a_v2_cooked_motion_database() {
        use regia_animation::motion_matching::{COOKED_MOTIONDB_VERSION, MotionDatabase};
        use regia_asset::{AssetId, COOKED_FORMAT_VERSION, CookedHeader, encode_cooked_file};

        let dir = regia_test_utils::unique_temp_dir("upgrade-motiondb-cooked");
        // The frozen v2 payload of the old-bytes corpus (ADR-2743).
        let v2: &[u8] =
            include_bytes!("../../../regia_agent/tests/fixtures/format-corpus/motiondb-cooked/v2.bin");
        assert_eq!(MotionDatabase::cooked_payload_version(v2).expect("v2"), 2);
        let header = CookedHeader {
            format_version: COOKED_FORMAT_VERSION,
            type_tag: regia_animation::cooked::motion_database_type_tag(),
            asset_id: AssetId::sub_asset(AssetId::default(), "locomotion.motiondb.ron"),
            importer_version: 1,
            dependencies: vec![],
            payload_len: v2.len() as u64,
        };
        let file = dir.join("locomotion.fckd");
        let old = encode_cooked_file(&header, v2).expect("encode");
        std::fs::write(&file, &old).expect("write");

        let plan = plan_with(&dir, &formats()).expect("plan");
        let formats_found: Vec<&str> = plan.iter().map(|p| p.format).collect();
        assert_eq!(formats_found, ["motiondb-cooked"]);
        assert!(plan[0].needs_migration());
        let args = UpgradeProjectArgs {
            project: dir.clone(),
            write: true,
            engine: None,
            json: false,
            backup: None,
        };
        run_with(&args, &formats()).expect("--write migrates the v2 database");
        let migrated = std::fs::read(&file).expect("read");
        assert_eq!(
            regia_animation::cooked::cooked_motion_database_file_version(&migrated).expect("probe"),
            Some(COOKED_MOTIONDB_VERSION)
        );
        let (kept, loaded) = regia_animation::cooked::load_cooked_motion_database(&migrated).expect("load");
        assert_eq!((kept.asset_id, kept.type_tag), (header.asset_id, header.type_tag));
        assert_eq!(loaded.clip_speeds().len(), loaded.len());
        let original = MotionDatabase::from_cooked_payload(v2).expect("v2 loads");
        assert_eq!(loaded.len(), original.len());
        assert_eq!(loaded.mean_scale_hash(), original.mean_scale_hash());
        assert_eq!(std::fs::read(dir.join("locomotion.fckd.bak")).expect("bak"), old);
        // A second run changes nothing.
        assert!(
            plan_with(&dir, &formats())
                .expect("plan")
                .iter()
                .all(|p| !p.needs_migration())
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `--write` on a v1 `.matgraph.ron`: rewritten at the current version, the `.bak` keeps the v1
    /// original, and the rewritten file reads back as the same graph the loader builds from v1.
    #[test]
    fn write_upgrades_v1_matgraph_and_keeps_the_original_as_bak() {
        let dir = regia_test_utils::unique_temp_dir("upgrade-matgraph");
        let file = dir.join("rock.matgraph.ron");
        let v1 = r#"(
        format_version: 1,
        nodes: { "c": Vec3((0.2, 0.3, 0.4)) },
        output: (base_color: Some("c")),
    )"#;
        std::fs::write(&file, v1).expect("fixture");
        let expected = regia_matgraph::MaterialGraph::from_ron(v1).expect("v1 loads");

        let args = UpgradeProjectArgs {
            project: dir.clone(),
            write: true,
            engine: None,
            json: false,
            backup: None,
        };
        run_with(&args, &formats()).expect("--write migrates the v1 matgraph");

        let migrated = std::fs::read_to_string(&file).expect("migrated file");
        assert_eq!(
            read_ron_version(migrated.as_bytes()).expect("migrated version"),
            regia_matgraph::MATGRAPH_FORMAT_VERSION,
            "the file on disk is rewritten, not just migrated in memory"
        );
        assert_eq!(
            regia_matgraph::MaterialGraph::from_ron(&migrated).expect("reparse"),
            expected
        );
        let backup = std::fs::read_to_string(dir.join("rock.matgraph.ron.bak")).expect("backup");
        assert_eq!(backup, v1, "the .bak is the untouched v1 original");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `--write` on a v1 `.matfunc.ron` and a v2 `.matgraph.ron` (the shapes before the
    /// `Noise` node, ADR-2318): both rewritten at the current version, same content, `.bak`s kept.
    #[test]
    fn write_upgrades_v1_matfunc_and_v2_matgraph() {
        let dir = regia_test_utils::unique_temp_dir("upgrade-matfunc");
        let func_file = dir.join("dim.matfunc.ron");
        let func_v1 = r#"(
        format_version: 1,
        name: "dim",
        inputs: [(name: "c", ty: Vec3)],
        nodes: { "c": Input("c"), "k": Float(0.5), "o": Mul("c", "k") },
        outputs: { "o": "o" },
    )"#;
        let graph_file = dir.join("rock.matgraph.ron");
        let graph_v2 = r#"(
        format_version: 2,
        functions: { "dim": File("dim.matfunc.ron") },
        nodes: {
            "c": Vec3((0.2, 0.3, 0.4)),
            "d": Call(function: "dim", args: { "c": "c" }),
        },
        output: (base_color: Some("d")),
    )"#;
        std::fs::write(&func_file, func_v1).expect("fixture");
        std::fs::write(&graph_file, graph_v2).expect("fixture");
        let func_expected = regia_matgraph::MaterialFunction::from_ron(func_v1).expect("v1 loads");
        let graph_expected = regia_matgraph::MaterialGraph::from_ron(graph_v2).expect("v2 loads");

        let args = UpgradeProjectArgs {
            project: dir.clone(),
            write: true,
            engine: None,
            json: false,
            backup: None,
        };
        run_with(&args, &formats()).expect("--write migrates both");

        let func = std::fs::read_to_string(&func_file).expect("migrated function");
        assert_eq!(
            read_ron_version(func.as_bytes()).expect("version"),
            regia_matgraph::MATFUNC_FORMAT_VERSION
        );
        assert_eq!(regia_matgraph::MATFUNC_FORMAT_VERSION, 4);
        assert_eq!(
            regia_matgraph::MaterialFunction::from_ron(&func).expect("reparse"),
            func_expected
        );
        let graph = std::fs::read_to_string(&graph_file).expect("migrated graph");
        assert_eq!(
            read_ron_version(graph.as_bytes()).expect("version"),
            regia_matgraph::MATGRAPH_FORMAT_VERSION
        );
        assert_eq!(regia_matgraph::MATGRAPH_FORMAT_VERSION, 5);
        assert_eq!(
            regia_matgraph::MaterialGraph::from_ron(&graph).expect("reparse"),
            graph_expected
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("dim.matfunc.ron.bak")).expect("backup"),
            func_v1
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("rock.matgraph.ron.bak")).expect("backup"),
            graph_v2
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `--write` on a v1 `.matinst.ron`, same contract as the matgraph case.
    #[test]
    fn write_upgrades_v1_matinst_and_keeps_the_original_as_bak() {
        let dir = regia_test_utils::unique_temp_dir("upgrade-matinst");
        let file = dir.join("rock_wet.matinst.ron");
        let v1 = "(format_version: 1, parent: \"rock.matgraph.ron\")";
        std::fs::write(&file, v1).expect("fixture");
        let expected = regia_matgraph::MaterialInstance::from_ron(v1).expect("v1 loads");

        let args = UpgradeProjectArgs {
            project: dir.clone(),
            write: true,
            engine: None,
            json: false,
            backup: None,
        };
        run_with(&args, &formats()).expect("--write migrates the v1 matinst");

        let migrated = std::fs::read_to_string(&file).expect("migrated file");
        assert_eq!(
            read_ron_version(migrated.as_bytes()).expect("migrated version"),
            regia_matgraph::MATINST_FORMAT_VERSION
        );
        assert_eq!(
            regia_matgraph::MaterialInstance::from_ron(&migrated).expect("reparse"),
            expected
        );
        let backup = std::fs::read_to_string(dir.join("rock_wet.matinst.ron.bak")).expect("backup");
        assert_eq!(backup, v1);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `--write` on a v2 `.matgraph.ron` (before the `Wind` node): rewritten at the current
    /// version, the `.bak` keeps the v2 original, and the rewritten file reads back as the graph
    /// the loader builds.
    #[test]
    fn write_upgrades_v2_matgraph_and_keeps_the_original_as_bak() {
        let dir = regia_test_utils::unique_temp_dir("upgrade-matgraph-v2");
        let file = dir.join("reed.matgraph.ron");
        let v2 = r#"(
        format_version: 2,
        nodes: {
            "t": Time,
            "wave": Sin("t"),
            "lift": Vec3((0.0, 0.0, 0.05)),
            "offset": Mul("lift", "wave"),
        },
        output: (world_position_offset: Some("offset")),
        shading: (max_vertex_offset: 0.05),
    )"#;
        std::fs::write(&file, v2).expect("fixture");
        let expected = regia_matgraph::MaterialGraph::from_ron(v2).expect("v2 loads");

        let args = UpgradeProjectArgs {
            project: dir.clone(),
            write: true,
            engine: None,
            json: false,
            backup: None,
        };
        run_with(&args, &formats()).expect("--write migrates the v2 matgraph");

        let migrated = std::fs::read_to_string(&file).expect("migrated file");
        assert_eq!(
            read_ron_version(migrated.as_bytes()).expect("migrated version"),
            regia_matgraph::MATGRAPH_FORMAT_VERSION,
            "the file on disk is rewritten at the current version"
        );
        assert_eq!(
            regia_matgraph::MaterialGraph::from_ron(&migrated).expect("reparse"),
            expected
        );
        let backup = std::fs::read_to_string(dir.join("reed.matgraph.ron.bak")).expect("backup");
        assert_eq!(backup, v2, "the .bak is the untouched v2 original");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `--write` on a v1 `.matfunc.ron`: rewritten at the current version (v2 came with the
    /// `Wind` node), the `.bak` keeps the v1 original, and the rewritten file reads back as the
    /// same function.
    #[test]
    fn write_upgrades_v1_matfunc_and_keeps_the_original_as_bak() {
        let dir = regia_test_utils::unique_temp_dir("upgrade-matfunc");
        let file = dir.join("weather.matfunc.ron");
        let v1 = r#"(
        format_version: 1,
        inputs: [(name: "c", ty: Vec3)],
        nodes: { "c": Input("c"), "k": Float(0.35), "out": Mul("c", "k") },
        outputs: { "out": "out" },
    )"#;
        std::fs::write(&file, v1).expect("fixture");
        let expected = regia_matgraph::MaterialFunction::from_ron(v1).expect("v1 loads");

        let args = UpgradeProjectArgs {
            project: dir.clone(),
            write: true,
            engine: None,
            json: false,
            backup: None,
        };
        run_with(&args, &formats()).expect("--write migrates the v1 matfunc");

        let migrated = std::fs::read_to_string(&file).expect("migrated file");
        assert_eq!(
            read_ron_version(migrated.as_bytes()).expect("migrated version"),
            regia_matgraph::MATFUNC_FORMAT_VERSION,
            "the file on disk is rewritten at the current version"
        );
        assert_eq!(
            regia_matgraph::MaterialFunction::from_ron(&migrated).expect("reparse"),
            expected
        );
        let backup = std::fs::read_to_string(dir.join("weather.matfunc.ron.bak")).expect("backup");
        assert_eq!(backup, v1, "the .bak is the untouched v1 original");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A `.vfx.ron` at the current VFX format version must plan as up to date; the registry
    /// entry for `vfx` still says v1, so a v4 file is reported as newer than the engine.
    #[test]
    fn a_current_vfx_file_plans_as_up_to_date() {
        let dir = regia_test_utils::unique_temp_dir("upgrade-vfx");
        // The version peek reads only the version field, so a bare header is enough.
        let header = format!("(version: {})", regia_vfx::VFX_FORMAT_VERSION);
        std::fs::write(dir.join("spark.vfx.ron"), header).expect("fixture");
        let plan = plan_with(&dir, &formats()).expect("plan");
        let vfx: Vec<&FilePlan> = plan.iter().filter(|p| p.format == "vfx").collect();
        assert_eq!(vfx.len(), 1);
        assert_eq!(vfx[0].found, regia_vfx::VFX_FORMAT_VERSION);
        assert!(!vfx[0].needs_migration());
        assert_eq!(vfx[0].status(), "up to date");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Spec 12 § 9 (M4): `regia upgrade-project` runs the no-op v1 `.meta` step through its format
    /// registry: a current sidecar plans as up to date, and `--write` leaves it byte for byte.
    #[test]
    fn a_current_meta_sidecar_is_a_no_op_upgrade() {
        let dir = regia_test_utils::unique_temp_dir("upgrade-meta-noop");
        let text = format!(
            "(\n    format_version: {},\n    id: \"67e55044-10b1-426f-9247-bb680e5fe0c8\",\n    importer: \"png\",\n)\n",
            regia_asset::META_FORMAT_VERSION
        );
        let meta = dir.join("rock.png.meta");
        std::fs::write(&meta, &text).expect("fixture");
        let plan = plan_with(&dir, &formats()).expect("plan");
        let metas: Vec<&FilePlan> = plan.iter().filter(|p| p.format == "meta").collect();
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].found, regia_asset::META_FORMAT_VERSION);
        assert!(!metas[0].needs_migration());
        assert_eq!(metas[0].status(), "up to date");
        let args = UpgradeProjectArgs {
            project: dir.clone(),
            write: true,
            engine: None,
            json: false,
            backup: None,
        };
        run_with(&args, &formats()).expect("--write with nothing to migrate");
        assert_eq!(std::fs::read_to_string(&meta).expect("read back"), text);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// ADR-7819: `.meta` v2 adds the `lossless_clips` import setting. A v1 sidecar plans as
    /// `v1 -> v2`; `--write` turns the switch off (a v1 importer ignored or refused the key) and
    /// keeps every other byte, comments included, of a file that never named it.
    #[test]
    fn a_v1_meta_upgrades_to_v2_with_the_lossless_clip_switch_off() {
        let dir = regia_test_utils::unique_temp_dir("upgrade-meta-v1-clips");
        let named = "(\n    format_version: 1,\n    id: \"67e55044-10b1-426f-9247-bb680e5fe0c8\",\n    \
                     importer: \"gltf\",\n    settings: \"(lossless_clips: true, meshlets: true)\",\n)\n";
        let plain = "// hand-written\n(\n    format_version: 1,\n    id: \"77e55044-10b1-426f-9247-bb680e5fe0c8\",\n    \
                     importer: \"bvh\",\n    settings: \"(end_sites: true)\",\n)\n";
        std::fs::write(dir.join("rig.glb.meta"), named).expect("fixture");
        std::fs::write(dir.join("walk.bvh.meta"), plain).expect("fixture");
        let plan = plan_with(&dir, &formats()).expect("plan");
        let metas: Vec<&FilePlan> = plan.iter().filter(|p| p.format == "meta").collect();
        assert_eq!(metas.len(), 2);
        assert!(metas.iter().all(|m| m.found == 1 && m.needs_migration()));
        assert_eq!(regia_asset::META_FORMAT_VERSION, 2);
        let args = UpgradeProjectArgs {
            project: dir.clone(),
            write: true,
            engine: None,
            json: false,
            backup: None,
        };
        run_with(&args, &formats()).expect("--write");

        let upgraded = std::fs::read(dir.join("rig.glb.meta")).expect("read back");
        let meta = regia_asset::MetaSidecar::from_ron(&upgraded).expect("parses");
        let text = String::from_utf8(upgraded).expect("utf-8");
        assert!(text.contains("format_version: 2"), "{text}");
        assert!(!text.contains("lossless_clips"), "the switch is off: {text}");
        assert!(!meta.lossless_clips());
        assert_eq!(meta.settings.as_deref(), Some("(meshlets: true)"));

        let upgraded = std::fs::read_to_string(dir.join("walk.bvh.meta")).expect("read back");
        assert_eq!(upgraded, plain.replace("format_version: 1", "format_version: 2"));
        // A second run has nothing to do.
        let plan = plan_with(&dir, &formats()).expect("plan");
        assert!(
            plan.iter()
                .filter(|p| p.format == "meta")
                .all(|m| !m.needs_migration())
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// An older vfx file plans as needing migration.
    #[test]
    fn an_older_vfx_file_is_not_called_up_to_date() {
        let dir = regia_test_utils::unique_temp_dir("upgrade-vfx-old");
        std::fs::write(dir.join("old.vfx.ron"), "(version: 0)").expect("fixture");
        let plan = plan_with(&dir, &formats()).expect("plan");
        let vfx: Vec<&FilePlan> = plan.iter().filter(|p| p.format == "vfx").collect();
        assert_eq!(vfx.len(), 1);
        assert_eq!(vfx[0].found, 0);
        assert!(vfx[0].needs_migration());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `.flmap` lightmaps (ADR-860) are listed by their `FLMP` header version; since ADR-1556 v1
    /// needs the v1 → v2 step, and the registered migrator turns the checked-in v1 fixture into a
    /// current file.
    #[test]
    fn lightmaps_are_listed_by_header_version() {
        let dir = regia_test_utils::unique_temp_dir("regia_cli_upgrade_lightmap");
        let mut v1 = Vec::from(regia_asset::lightmap::LIGHTMAP_MAGIC);
        v1.extend_from_slice(&1u32.to_le_bytes());
        v1.extend_from_slice(&[0; 8]);
        let file = dir.join("bore.flmap");
        let tmp = dir.join("bore.flmap.tmp");
        std::fs::write(&tmp, &v1).expect("write");
        std::fs::rename(&tmp, &file).expect("rename");
        let plan = plan_with(&dir, &formats()).expect("plan");
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].format, "lightmap");
        assert_eq!(plan[0].found, 1);
        assert_eq!(plan[0].current, regia_asset::lightmap::LIGHTMAP_FORMAT_VERSION);
        assert!(plan[0].needs_migration());
        let registry = formats();
        let format = registry
            .iter()
            .find(|f| f.name == "lightmap")
            .expect("lightmap is registered");
        // A header-only stub does not decode: the migrator says why instead of writing junk.
        assert!((format.migrate)(&v1, 1).is_err());
        let fixture: &[u8] = include_bytes!("../../../regia_asset/tests/fixtures/lightmap-v1.flmap");
        let migrated = (format.migrate)(fixture, 1).expect("v1 fixture migrates");
        assert_eq!(
            regia_asset::lightmap::lightmap_version(&migrated).expect("version"),
            regia_asset::lightmap::LIGHTMAP_FORMAT_VERSION
        );
        assert_eq!(
            regia_asset::lightmap::Lightmap::from_bytes(&migrated).expect("reads"),
            regia_asset::lightmap::Lightmap::from_bytes(fixture).expect("reads")
        );
        // Version 2 (ADR-1556) to the current one (ADR-2024): the same bake, with no entry key.
        let fixture: &[u8] = include_bytes!("../../../regia_asset/tests/fixtures/lightmap-v2.flmap");
        let migrated = (format.migrate)(fixture, 2).expect("v2 fixture migrates");
        assert_eq!(
            regia_asset::lightmap::Lightmap::from_bytes(&migrated).expect("reads"),
            regia_asset::lightmap::Lightmap::from_bytes(fixture).expect("reads")
        );
        // Version 3 (ADR-2024) to the current one (ADR-2107): the same bake, with no clusters.
        let fixture: &[u8] = include_bytes!("../../../regia_asset/tests/fixtures/lightmap-v3.flmap");
        let migrated = (format.migrate)(fixture, 3).expect("v3 fixture migrates");
        assert_eq!(
            regia_asset::lightmap::Lightmap::from_bytes(&migrated).expect("reads"),
            regia_asset::lightmap::Lightmap::from_bytes(fixture).expect("reads")
        );
        // Version 4 (ADR-2107) to the current one (ADR-2314): the same bake and clusters, RGB9E5
        // texels, no denoise record.
        let fixture: &[u8] = include_bytes!("../../../regia_asset/tests/fixtures/lightmap-v4.flmap");
        let migrated = (format.migrate)(fixture, 4).expect("v4 fixture migrates");
        let back = regia_asset::lightmap::Lightmap::from_bytes(&migrated).expect("reads");
        assert_eq!(
            back,
            regia_asset::lightmap::Lightmap::from_bytes(fixture).expect("reads")
        );
        assert_eq!(
            back.texel_encoding(),
            regia_asset::lightmap::LightmapTexelEncoding::Rgb9e5
        );
        assert_eq!(back.denoise, None);
        // Versions 5 and 6 to the current one (ADR-3300): one page, the same bake, the same sections.
        for (found, fixture) in [
            (
                5u32,
                &include_bytes!("../../../regia_asset/tests/fixtures/lightmap-v5.flmap")[..],
            ),
            (
                6,
                &include_bytes!("../../../regia_asset/tests/fixtures/lightmap-v6.flmap")[..],
            ),
        ] {
            let migrated = (format.migrate)(fixture, found).expect("migrates");
            let back = regia_asset::lightmap::Lightmap::from_bytes(&migrated).expect("reads");
            assert_eq!(back.pages(), 1, "v{found}");
            assert_eq!(
                back,
                regia_asset::lightmap::Lightmap::from_bytes(fixture).expect("reads"),
                "v{found}"
            );
            assert_eq!(
                regia_asset::lightmap::Lightmap::read_header(&migrated)
                    .expect("header")
                    .payload_hash,
                regia_asset::lightmap::Lightmap::read_header(fixture)
                    .expect("header")
                    .payload_hash,
                "v{found}: the sections are copied"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `--write` on a v1 `.ui.ron` (ADR-1381): rewritten at the current version (v3 since the UI
    /// designer) with only the version number changed, the `.bak` keeps the v1 original, and the
    /// result loads as the same tree.
    #[test]
    fn write_upgrades_v1_ui_and_keeps_the_original_as_bak() {
        let dir = regia_test_utils::unique_temp_dir("upgrade-ui");
        let file = dir.join("hud.ui.ron");
        let v1 = "// hud\n(\n    version: 1,\n    root: (name: \"hud\", children: [(widget: button)]),\n)\n";
        std::fs::write(&file, v1).expect("fixture");
        let expected =
            regia_ui::parse_ui_ron(v1, "hud", &regia_ui::UiMigrators::default()).expect("v1 loads");

        let plan = plan_with(&dir, &formats()).expect("plan");
        let ui: Vec<&FilePlan> = plan.iter().filter(|p| p.format == "ui").collect();
        assert_eq!(ui.len(), 1);
        assert_eq!((ui[0].found, ui[0].current), (1, regia_ui::UI_RON_VERSION));
        assert!(ui[0].needs_migration());

        let args = UpgradeProjectArgs {
            project: dir.clone(),
            write: true,
            engine: None,
            json: false,
            backup: None,
        };
        run_with(&args, &formats()).expect("--write migrates the v1 ui");

        let migrated = std::fs::read_to_string(&file).expect("migrated file");
        assert_eq!(
            migrated,
            v1.replacen("version: 1", &format!("version: {}", regia_ui::UI_RON_VERSION), 1)
        );
        assert_eq!(
            read_ron_version(migrated.as_bytes()).expect("migrated version"),
            regia_ui::UI_RON_VERSION
        );
        let reparsed =
            regia_ui::parse_ui_ron(&migrated, "hud", &regia_ui::UiMigrators::default()).expect("reparse");
        assert_eq!(format!("{reparsed:?}"), format!("{expected:?}"));
        let backup = std::fs::read_to_string(dir.join("hud.ui.ron.bak")).expect("backup");
        assert_eq!(backup, v1, "the .bak is the untouched v1 original");

        let again = plan_with(&dir, &formats()).expect("plan");
        let ui: Vec<&FilePlan> = again.iter().filter(|p| p.format == "ui").collect();
        assert_eq!(ui.len(), 1, "the .bak is not planned as a .ui.ron");
        assert!(!ui[0].needs_migration());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A v7 `.ui.ron` with a raw-id mask (ADR-3148): planned for migration, rewritten at the current
    /// version with only the number changed, and the mask still names the same renderer id.
    #[test]
    fn write_upgrades_v7_ui_to_the_current_version() {
        let dir = regia_test_utils::unique_temp_dir("upgrade-ui-v7");
        let file = dir.join("hud.ui.ron");
        let v7 =
            "(\n    version: 7,\n    root: (children: [(name: \"m\", mask: (image: 7, fit: contain))]),\n)\n";
        std::fs::write(&file, v7).expect("fixture");
        let plan = plan_with(&dir, &formats()).expect("plan");
        let ui: Vec<&FilePlan> = plan.iter().filter(|p| p.format == "ui").collect();
        assert_eq!((ui[0].found, ui[0].current), (7, regia_ui::UI_RON_VERSION));
        let args = UpgradeProjectArgs {
            project: dir.clone(),
            write: true,
            engine: None,
            json: false,
            backup: None,
        };
        run_with(&args, &formats()).expect("--write migrates the v7 ui");
        let migrated = std::fs::read_to_string(&file).expect("migrated");
        assert_eq!(
            migrated,
            v7.replacen("version: 7", &format!("version: {}", regia_ui::UI_RON_VERSION), 1)
        );
        let back =
            regia_ui::parse_ui_ron(&migrated, "hud", &regia_ui::UiMigrators::default()).expect("current");
        assert_eq!(back.root.children[0].mask.as_ref().and_then(|m| m.image), Some(7));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `.ui.ron` v10 → v11 (ADR-9980): `--write` rewrites a v10 file with a two-element text input
    /// and a `preserve_aspect` image at v11 with only the version number changed (comments kept),
    /// and the result reads the old text input with no options and the image as `contain`.
    #[test]
    fn write_upgrades_v10_ui_text_input_and_image_to_v11() {
        let dir = regia_test_utils::unique_temp_dir("upgrade-ui-v10");
        let file = dir.join("form.ui.ron");
        let v10 = "// sign-in form\n(\n    version: 10, // v10\n    root: (children: [\n        // name\n        \
                   (name: \"user\", widget: text_input(\"Name\", false)),\n        \
                   (name: \"logo\", image: (id: 3)),\n    ]),\n)\n";
        std::fs::write(&file, v10).expect("fixture");
        let plan = plan_with(&dir, &formats()).expect("plan");
        let ui: Vec<&FilePlan> = plan.iter().filter(|p| p.format == "ui").collect();
        assert_eq!((ui[0].found, ui[0].current), (10, 11));
        assert!(ui[0].needs_migration());
        let args = UpgradeProjectArgs {
            project: dir.clone(),
            write: true,
            engine: None,
            json: false,
            backup: None,
        };
        run_with(&args, &formats()).expect("--write migrates the v10 ui");
        let migrated = std::fs::read_to_string(&file).expect("migrated");
        assert_eq!(migrated, v10.replacen("version: 10,", "version: 11,", 1));
        let back = regia_ui::parse_ui_ron(&migrated, "form", &regia_ui::UiMigrators::default()).expect("v11");
        let Some(regia_ui::authoring::WidgetDef::TextInput(placeholder, multiline, options)) =
            &back.root.children[0].widget
        else {
            panic!("a text input");
        };
        assert_eq!((placeholder.as_str(), *multiline), ("Name", false));
        assert!(options.is_default());
        let image = back.root.children[1].image.as_ref().expect("image");
        assert_eq!(image.effective_fit(), regia_ui::ImageFit::Contain);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A v1 `.ui.ron` whose body the loader rejects fails the migration and is left untouched.
    #[test]
    fn a_broken_v1_ui_is_not_rewritten() {
        let v1 = b"(version: 1, root: (naem: \"typo\"))";
        let err = migrate_ui(v1, 1).expect_err("unknown field");
        assert!(err.contains("naem"), "{err}");
        assert!(migrate_ui(b"(version: 1, root: ())", 1).is_ok());
    }

    /// `.wstream` v1 → v2 (ADR-2483): `--write` rewrites a cooked v1 world index at v2 with no
    /// atlases (backup kept) and leaves a current one and other cooked files alone.
    #[test]
    fn write_upgrades_v1_world_indexes_to_v2() {
        use regia_world::format::{WORLD_INDEX_FORMAT_VERSION, WorldIndex, world_index_file_version};
        let dir = regia_test_utils::unique_temp_dir("regia-upgrade-wstream");
        let cache = dir.join(".regia/cooked/linux");
        std::fs::create_dir_all(&cache).expect("tmp dir");
        let index = |n: u128| WorldIndex {
            format_version: WORLD_INDEX_FORMAT_VERSION,
            level: regia_core::id::Guid::from_u128(n),
            content_hash: [0; 32],
            settings: regia_scene::streaming::LevelStreaming {
                enabled: true,
                ..Default::default()
            },
            cells: vec![],
            bounds: [regia_scene::stream_grid::CellCoord::new(0, 0); 2],
            hlod: None,
            replicated: vec![],
            nav: None,
        };
        let old = index(1).to_v1_bytes().expect("v1");
        let current = index(2).to_bytes().expect("v2");
        std::fs::write(cache.join("old.fckd"), &old).expect("write");
        std::fs::write(cache.join("current.fckd"), &current).expect("write");
        let args = UpgradeProjectArgs {
            project: dir.clone(),
            write: true,
            engine: None,
            json: false,
            backup: None,
        };
        run_with(&args, &formats()).expect("--write");
        let migrated = std::fs::read(cache.join("old.fckd")).expect("old");
        assert_eq!(world_index_file_version(&migrated).expect("probe"), Some(2));
        assert_eq!(WorldIndex::decode_any(&migrated).expect("v2"), index(1));
        assert_eq!(std::fs::read(cache.join("old.fckd.bak")).expect("backup"), old);
        assert_eq!(
            std::fs::read(cache.join("current.fckd")).expect("current"),
            current
        );
        assert!(!cache.join("current.fckd.bak").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// ADR-2411: `--write` moves a v11 level to v12 naming no terrain (its side files keep the
    /// level's name, so it loads the same terrain), and a `.grassdensity.bin` at v1 is current.
    #[test]
    fn write_upgrades_a_v11_level_to_v12_and_reads_grass_density_versions() {
        let dir = regia_test_utils::unique_temp_dir("upgrade-level-v12");
        std::fs::create_dir_all(&dir).expect("tmp dir");
        let file = dir.join("arena.level.ron");
        std::fs::write(
            &file,
            r#"SceneFile(
    format_version: 11,
    entities: [],
    resources: {
        "regia_scene::level::LevelSettings": (look: (exposure_bias_ev: 0.5)),
    },
)"#,
        )
        .expect("fixture");
        let density = regia_terrain::GrassDensityPaint::new(8).expect("density");
        std::fs::write(
            dir.join("arena.grassdensity.bin"),
            density.to_bytes().expect("bytes"),
        )
        .expect("density");
        let args = UpgradeProjectArgs {
            project: dir.clone(),
            write: true,
            engine: None,
            json: false,
            backup: None,
        };
        run_with(&args, &formats()).expect("--write migrates the v11 level");
        let migrated = std::fs::read_to_string(&file).expect("read");
        assert_eq!(
            read_ron_version(migrated.as_bytes()).expect("version"),
            regia_scene::format::SCENE_FORMAT_VERSION
        );
        let level =
            regia_scene::level::Level::from_ron_str(regia_scene::level::LevelId::default(), &migrated)
                .expect("the migrated level reads");
        assert_eq!(level.settings.terrain, None, "a v11 level names no terrain");
        assert!(!migrated.contains("terrain"), "{migrated}");
        let bytes = std::fs::read(dir.join("arena.grassdensity.bin")).expect("density");
        assert_eq!(
            postcard_version(&bytes).expect("probe"),
            Some(regia_terrain::GRASS_DENSITY_FORMAT_VERSION)
        );
        assert!(
            !dir.join("arena.grassdensity.bin.bak").exists(),
            "a current file is not rewritten"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// ADR-2860: `--write` moves a v12 level to v13 naming no capture bake, and a `.fcap` at v1
    /// is current.
    #[test]
    fn write_upgrades_a_v12_level_to_v13_and_reads_fcap_versions() {
        let dir = regia_test_utils::unique_temp_dir("upgrade-level-v13");
        let file = dir.join("hall.level.ron");
        std::fs::write(
            &file,
            r#"SceneFile(
    format_version: 12,
    entities: [],
    resources: {
        "regia_scene::level::LevelSettings": (look: (exposure_bias_ev: 0.5)),
    },
)"#,
        )
        .expect("fixture");
        let bake = regia_asset::local_capture_bake::FcapFile {
            width: 4,
            captures: vec![],
        };
        std::fs::write(dir.join("hall.fcap"), bake.to_bytes().expect("bytes")).expect("fcap");
        let args = UpgradeProjectArgs {
            project: dir.clone(),
            write: true,
            engine: None,
            json: false,
            backup: None,
        };
        run_with(&args, &formats()).expect("--write migrates the v12 level");
        let migrated = std::fs::read_to_string(&file).expect("read");
        assert_eq!(
            read_ron_version(migrated.as_bytes()).expect("version"),
            regia_scene::format::SCENE_FORMAT_VERSION
        );
        let level =
            regia_scene::level::Level::from_ron_str(regia_scene::level::LevelId::default(), &migrated)
                .expect("the migrated level reads");
        assert_eq!(
            level.settings.look.captures, None,
            "a v12 level names no capture bake"
        );
        assert!(!migrated.contains("captures"), "{migrated}");
        assert!(
            !dir.join("hall.fcap.bak").exists(),
            "a current bake is not rewritten"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// ADR-10079: `--write` moves a v13 level that names a terrain to v14 with `sample_layout:
    /// VertexCentered` and every other terrain field kept; a second `--write` changes no byte
    /// (the migration is idempotent).
    #[test]
    fn write_upgrades_a_v13_terrain_level_to_v14_once() {
        let dir = regia_test_utils::unique_temp_dir("upgrade-level-v14");
        let file = dir.join("ridge.level.ron");
        std::fs::write(
            &file,
            r#"SceneFile(
    format_version: 13,
    entities: [],
    resources: {
        "regia_scene::level::LevelSettings": (terrain: Some((
            layers: "maps/ridge/terrain.layers.ron",
            map: "maps/ridge",
            world_width_m: Some(1280.0),
            height_scale_m: 300.0,
            height_offset_m: -12.0,
        ))),
    },
)"#,
        )
        .expect("fixture");
        let args = UpgradeProjectArgs {
            project: dir.clone(),
            write: true,
            engine: None,
            json: false,
            backup: None,
        };
        run_with(&args, &formats()).expect("--write migrates the v13 level");
        let once = std::fs::read(&file).expect("read");
        assert_eq!(read_ron_version(&once).expect("version"), 14);
        let text = String::from_utf8(once.clone()).expect("utf-8");
        assert!(text.contains("sample_layout: VertexCentered"), "{text}");
        let level = regia_scene::level::Level::from_ron_str(regia_scene::level::LevelId::default(), &text)
            .expect("the migrated level reads");
        let terrain = level.settings.terrain.expect("terrain kept");
        assert_eq!(
            terrain.sample_layout,
            regia_scene::level::TerrainSampleLayout::VertexCentered
        );
        assert_eq!(terrain.world_width_m, Some(1280.0));
        run_with(&args, &formats()).expect("a second --write");
        assert_eq!(
            std::fs::read(&file).expect("read"),
            once,
            "the second run changes nothing"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A launcher crate as the templates wrote it before ADR-2645: no build script, no
    /// build-dependencies.
    const OLD_LAUNCHER: &str = "[package]\nname = \"g_launcher\"\nversion.workspace = true\n\n[[bin]]\nname = \"g\"\npath = \"src/main.rs\"\n\n[dependencies]\nregia = { path = \"../engine/crates/regia\" }\ng = { path = \"../game\" }\n";

    /// A project shaped like the blank template: the launcher, the editor and the Android crate.
    fn old_template_project(label: &str) -> PathBuf {
        let dir = regia_test_utils::unique_temp_dir(label);
        std::fs::write(
            dir.join("Regia.toml"),
            "[project]\nname = \"G\"\nid = \"com.example.g\"\n",
        )
        .expect("manifest");
        std::fs::write(
            dir.join("Cargo.toml"),
            "[workspace]\nmembers = [\"launcher\", \"editor\", \"android\"]\n",
        )
        .expect("workspace");
        for crate_dir in ["launcher/src", "editor/src", "android/src"] {
            std::fs::create_dir_all(dir.join(crate_dir)).expect("dirs");
        }
        std::fs::write(dir.join("launcher/Cargo.toml"), OLD_LAUNCHER).expect("launcher");
        std::fs::write(dir.join("launcher/src/main.rs"), "fn main() {}\n").expect("main");
        let editor = "[package]\nname = \"g_editor\"\n\n[dependencies]\nregia = { path = \"../engine/crates/regia\" }\n";
        std::fs::write(dir.join("editor/Cargo.toml"), editor).expect("editor");
        std::fs::write(dir.join("editor/src/main.rs"), "fn main() {}\n").expect("main");
        let android = "[package]\nname = \"g_android\"\n\n[lib]\ncrate-type = [\"cdylib\"]\n\n[dependencies]\nregia = { path = \"../engine/crates/regia\" }\n";
        std::fs::write(dir.join("android/Cargo.toml"), android).expect("android");
        std::fs::write(dir.join("android/src/lib.rs"), "").expect("lib");
        dir
    }

    fn write_args(project: &Path) -> UpgradeProjectArgs {
        UpgradeProjectArgs {
            project: project.to_path_buf(),
            write: true,
            engine: None,
            json: false,
            backup: None,
        }
    }

    /// ADR-2645: `--write` gives an old template project's launcher the Windows build script and
    /// its build-dependencies once; a second run changes nothing; the editor and the Android crate
    /// are not game executables.
    #[test]
    fn upgrade_project_adds_the_windows_build_script_once() {
        let dir = old_template_project("upgrade-build-script");
        let plans = plan_build_scripts(&dir);
        assert_eq!(
            plans,
            [BuildScriptPlan {
                dir: "launcher",
                action: BuildScriptAction::Add
            }]
        );
        let dry = UpgradeProjectArgs {
            write: false,
            ..write_args(&dir)
        };
        run_with(&dry, &[]).expect("dry run");
        assert!(
            !dir.join("launcher/build.rs").exists(),
            "a dry run writes nothing"
        );

        run_with(&write_args(&dir), &[]).expect("--write");
        assert_eq!(
            std::fs::read_to_string(dir.join("launcher/build.rs")).expect("build.rs"),
            regia_templates::WINDOWS_BUILD_SCRIPT
        );
        let manifest = std::fs::read_to_string(dir.join("launcher/Cargo.toml")).expect("manifest");
        assert!(
            manifest.starts_with(OLD_LAUNCHER),
            "the old text is kept: {manifest}"
        );
        assert!(
            manifest.ends_with(&regia_templates::windows_build_dependencies_table()),
            "{manifest}"
        );
        let parsed: toml::Table = manifest.parse().expect("still TOML");
        assert!(parsed["build-dependencies"].get("winresource").is_some());
        assert_eq!(
            std::fs::read_to_string(dir.join("launcher/Cargo.toml.bak")).expect("backup"),
            OLD_LAUNCHER
        );
        for other in ["editor/build.rs", "android/build.rs", "build.rs"] {
            assert!(!dir.join(other).exists(), "{other}");
        }

        // Idempotent: the second run plans `current` and writes no second backup.
        assert_eq!(plans_status(&dir), ["launcher/build.rs: current"]);
        run_with(&write_args(&dir), &[]).expect("second --write");
        assert_eq!(
            std::fs::read_to_string(dir.join("launcher/Cargo.toml")).expect("manifest"),
            manifest
        );
        assert!(!dir.join("launcher/Cargo.toml.bak.2").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The 0.2.23 template build script: the current one without the blocks ADR-5181 added.
    fn earlier_template_script() -> String {
        // The blocks ADR-5181 added (as in regia_templates' windows_build_tests).
        const ADR_5181_BLOCKS: [&str; 4] = [
            "//! It also gives the exe's main thread an 8 MiB stack, the size Linux gives it. Windows reserves\n\
             //! 1 MiB unless the exe asks for more, and a debug build of a template game overflows that while\n\
             //! it starts (seen on Windows 11, MSVC, 2026-11-10).\n\
             //!\n",
            "\n/// The main thread's stack reserve in bytes (the Linux default; Windows' own default is 1 MiB).\n\
             pub(crate) const MAIN_STACK_BYTES: u32 = 8 << 20;\n",
            "    let target_env = std::env::var(\"CARGO_CFG_TARGET_ENV\").unwrap_or_default();\n\
             \x20   if let Some(arg) = main_stack_link_arg(&target_env) {\n\
             \x20       println!(\"cargo:rustc-link-arg-bins={arg}\");\n\
             \x20   }\n",
            "/// The linker argument that sets the exe's main-thread stack reserve to [`MAIN_STACK_BYTES`]:\n\
             /// `/STACK` for the MSVC linker, `--stack` for MinGW `ld` and `lld`. Another toolchain gets none.\n\
             pub(crate) fn main_stack_link_arg(target_env: &str) -> Option<String> {\n\
             \x20   match target_env {\n\
             \x20       \"msvc\" => Some(format!(\"/STACK:{MAIN_STACK_BYTES}\")),\n\
             \x20       \"gnu\" => Some(format!(\"-Wl,--stack,{MAIN_STACK_BYTES}\")),\n\
             \x20       _ => None,\n\
             \x20   }\n\
             }\n\
             \n",
        ];
        // The blocks the corr-low-editor icon cap added (as in regia_templates' windows_build_tests).
        const ICON_CAP_BLOCKS: [&str; 2] = [
            "\n/// What a larger icon is pre-resampled to before the icon sizes are made: the largest size is\n\
             /// 256, so a 1024 source loses nothing, and the premultiplied f32 copy stays at 16 MB instead\n\
             /// of ~270 MB for a 4096 icon in a build script.\n\
             pub(crate) const ICON_SOURCE_CAP: u32 = 1024;\n\
             \n\
             /// The side `ico_from_rgba` resamples the fitted icon from: at most [`ICON_SOURCE_CAP`].\n\
             pub(crate) fn icon_source_side(width: u32) -> u32 {\n\
             \x20   width.min(ICON_SOURCE_CAP)\n\
             }\n",
            "    // Pre-resample a large source once so each size pass works from at most 1024 px (see\n\
             \x20   // `ICON_SOURCE_CAP`): memory stays at build-script scale.\n\
             \x20   let square = if square.width > ICON_SOURCE_CAP {\n\
             \x20       resize(&square, icon_source_side(square.width))\n\
             \x20   } else {\n\
             \x20       square\n\
             \x20   };\n",
        ];
        // The SPDX first line the script gained with ADR-10225.
        const SPDX_BLOCKS: [&str; 1] = ["// SPDX-License-Identifier: MIT-0\n"];
        let mut earlier = regia_templates::WINDOWS_BUILD_SCRIPT.to_owned();
        for block in SPDX_BLOCKS
            .into_iter()
            .chain(ADR_5181_BLOCKS)
            .chain(ICON_CAP_BLOCKS)
        {
            earlier = earlier.replacen(block, "", 1);
        }
        earlier
    }

    /// ADR-5181: an unchanged copy of the 0.2.23 template script (no main-thread stack size) is
    /// replaced by the current one; an edited copy stays the user's.
    #[test]
    fn upgrade_project_updates_an_earlier_template_build_script() {
        let earlier = earlier_template_script();
        assert!(regia_templates::is_earlier_windows_build_script(&earlier));

        let dir = old_template_project("upgrade-earlier-build-script");
        run_with(&write_args(&dir), &[]).expect("--write adds the script");
        let manifest = std::fs::read_to_string(dir.join("launcher/Cargo.toml")).expect("manifest");
        std::fs::write(dir.join("launcher/build.rs"), earlier.replace('\n', "\r\n")).expect("0.2.23 script");
        assert_eq!(plans_status(&dir), ["launcher/build.rs: update"]);
        run_with(&write_args(&dir), &[]).expect("--write updates it");
        assert_eq!(
            std::fs::read_to_string(dir.join("launcher/build.rs")).expect("build.rs"),
            regia_templates::WINDOWS_BUILD_SCRIPT
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("launcher/Cargo.toml")).expect("manifest"),
            manifest,
            "the manifest already had its build-dependencies"
        );
        assert_eq!(plans_status(&dir), ["launcher/build.rs: current"]);

        // An edited earlier copy is the user's.
        std::fs::write(dir.join("launcher/build.rs"), format!("{earlier}// mine\n")).expect("edited");
        assert_eq!(plans_status(&dir), ["launcher/build.rs: user_script"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Review finding 6 (ADR-5617): an unchanged earlier script whose manifest cannot take the
    /// build-dependencies by a text edit plans `manual`, as the current script does, not
    /// `user_script` (it is the engine's file, not the user's); `--write` leaves both alone.
    #[test]
    fn an_earlier_script_with_an_uneditable_manifest_plans_manual() {
        let dir = old_template_project("upgrade-earlier-manual");
        let manifest = format!("{OLD_LAUNCHER}\n[build-dependencies.cc]\nversion = \"1\"\n");
        assert_eq!(
            with_build_dependencies(&manifest),
            None,
            "no header line to extend"
        );
        std::fs::write(dir.join("launcher/Cargo.toml"), &manifest).expect("manifest");
        let earlier = earlier_template_script();
        std::fs::write(dir.join("launcher/build.rs"), &earlier).expect("0.2.23 script");
        assert_eq!(plans_status(&dir), ["launcher/build.rs: manual"]);
        run_with(&write_args(&dir), &[]).expect("--write");
        assert_eq!(
            std::fs::read_to_string(dir.join("launcher/build.rs")).expect("build.rs"),
            earlier
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("launcher/Cargo.toml")).expect("manifest"),
            manifest
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Review finding 3 (ADR-5617): a net-game project's dedicated server is a game executable
    /// too, so `--write` gives `server/build.rs` the template's script and its build-dependencies.
    #[test]
    fn upgrade_project_gives_the_net_game_server_the_build_script() {
        let dir = regia_test_utils::unique_temp_dir("upgrade-net-server");
        std::fs::write(
            dir.join("Regia.toml"),
            "[project]\nname = \"N\"\nid = \"com.example.n\"\n",
        )
        .expect("manifest");
        std::fs::write(
            dir.join("Cargo.toml"),
            "[workspace]\nmembers = [\"game\", \"server\", \"client\"]\n",
        )
        .expect("workspace");
        for side in ["server", "client"] {
            std::fs::create_dir_all(dir.join(side).join("src")).expect("dirs");
            let cargo = format!(
                "[package]\nname = \"n_{side}\"\n\n[dependencies]\nn = {{ path = \"../game\" }}\nregia = {{ path = \"../engine/crates/regia\" }}\n"
            );
            std::fs::write(dir.join(side).join("Cargo.toml"), cargo).expect("crate");
            std::fs::write(dir.join(side).join("src/main.rs"), "fn main() {}\n").expect("main");
        }
        assert_eq!(
            plans_status(&dir),
            ["client/build.rs: add", "server/build.rs: add"]
        );
        run_with(&write_args(&dir), &[]).expect("--write");
        assert_eq!(
            std::fs::read_to_string(dir.join("server/build.rs")).expect("server build.rs"),
            regia_templates::WINDOWS_BUILD_SCRIPT
        );
        let server = std::fs::read_to_string(dir.join("server/Cargo.toml")).expect("manifest");
        assert!(
            server.ends_with(&regia_templates::windows_build_dependencies_table()),
            "{server}"
        );
        assert_eq!(
            plans_status(&dir),
            ["client/build.rs: current", "server/build.rs: current"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Review finding 5 (ADR-5617): the line for a script written into an unchanged manifest
    /// names the reason of its action.
    #[test]
    fn an_added_script_names_the_icon_reason_and_an_updated_one_the_stack() {
        assert_eq!(
            added_script_reason(&BuildScriptAction::Add),
            "the Windows exe icon and version info, ADR-2645"
        );
        assert_eq!(
            added_script_reason(&BuildScriptAction::Update),
            "the exe's main-thread stack, ADR-5181"
        );
    }

    fn plans_status(dir: &Path) -> Vec<String> {
        plan_build_scripts(dir)
            .iter()
            .map(|p| format!("{}: {}", p.script(), p.status()))
            .collect()
    }

    /// ADR-2645: a build script the user wrote, or a `build =` key pointing elsewhere, is never
    /// replaced, and its manifest is not edited.
    #[test]
    fn upgrade_project_leaves_a_user_build_script_alone() {
        let dir = old_template_project("upgrade-user-build-script");
        let mine = "fn main() { println!(\"cargo:rustc-env=MINE=1\"); }\n";
        std::fs::write(dir.join("launcher/build.rs"), mine).expect("user build.rs");
        assert_eq!(plans_status(&dir), ["launcher/build.rs: user_script"]);
        run_with(&write_args(&dir), &[]).expect("--write");
        assert_eq!(
            std::fs::read_to_string(dir.join("launcher/build.rs")).expect("kept"),
            mine
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("launcher/Cargo.toml")).expect("kept"),
            OLD_LAUNCHER
        );
        assert!(!dir.join("launcher/Cargo.toml.bak").exists());

        // An edited copy of the template's script is the user's too.
        let edited = regia_templates::WINDOWS_BUILD_SCRIPT.replace(
            "ICON_SIZES: [u32; 4] = [16, 32, 48, 256]",
            "ICON_SIZES: [u32; 4] = [16, 24, 32, 256]",
        );
        assert_ne!(edited, regia_templates::WINDOWS_BUILD_SCRIPT);
        std::fs::write(dir.join("launcher/build.rs"), &edited).expect("edited");
        assert_eq!(plans_status(&dir), ["launcher/build.rs: user_script"]);

        // A crate whose `build` key names another script.
        std::fs::remove_file(dir.join("launcher/build.rs")).expect("rm");
        let custom = OLD_LAUNCHER.replace(
            "version.workspace = true\n",
            "version.workspace = true\nbuild = \"tools/build.rs\"\n",
        );
        std::fs::write(dir.join("launcher/Cargo.toml"), &custom).expect("custom build key");
        assert_eq!(plans_status(&dir), ["launcher/build.rs: user_script"]);
        run_with(&write_args(&dir), &[]).expect("--write");
        assert!(!dir.join("launcher/build.rs").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ADR-2645: a manifest with build-dependencies of its own gets only the missing lines, under
    /// its existing header; one that spells the table some other way is left to the user.
    #[test]
    fn existing_build_dependencies_are_extended_not_duplicated() {
        let text = format!(
            "{OLD_LAUNCHER}\n[build-dependencies]\npng = \"0.18.1\"\ncc = \"1\"\n\n[features]\nx = []\n"
        );
        let edited = with_build_dependencies(&text).expect("extended");
        let parsed: toml::Table = edited.parse().expect("TOML");
        let deps = parsed["build-dependencies"].as_table().expect("table");
        assert_eq!(deps["png"].as_str(), Some("0.18.1"), "the user's pin stays");
        assert_eq!(deps["cc"].as_str(), Some("1"));
        assert!(deps.contains_key("winresource") && deps.contains_key("toml"));
        assert_eq!(edited.matches("png =").count(), 1);
        assert!(parsed["features"].get("x").is_some());
        assert_eq!(
            with_build_dependencies(&edited).as_deref(),
            Some(edited.as_str()),
            "a fixed point"
        );

        let dotted = format!("build-dependencies = {{ cc = \"1\" }}\n{OLD_LAUNCHER}");
        assert_eq!(with_build_dependencies(&dotted), None);
        assert_eq!(with_build_dependencies("[package\n"), None, "not TOML");

        // The template's script present but its dependencies missing: only the manifest changes.
        let dir = old_template_project("upgrade-build-deps");
        std::fs::write(
            dir.join("launcher/build.rs"),
            regia_templates::WINDOWS_BUILD_SCRIPT,
        )
        .expect("ours");
        assert_eq!(plans_status(&dir), ["launcher/build.rs: dependencies"]);
        run_with(&write_args(&dir), &[]).expect("--write");
        assert_eq!(plans_status(&dir), ["launcher/build.rs: current"]);
        std::fs::write(dir.join("launcher/Cargo.toml"), &dotted).expect("dotted");
        assert_eq!(plans_status(&dir), ["launcher/build.rs: manual"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ADR-2645: the quick template's single crate and the net game's client are game executables
    /// too; a virtual workspace root is not.
    #[test]
    fn quick_crates_and_net_clients_get_the_build_script() {
        let quick = regia_test_utils::unique_temp_dir("upgrade-quick-build");
        std::fs::create_dir_all(quick.join("src")).expect("dirs");
        let manifest = "[package]\nname = \"q\"\nversion = \"0.1.0\"\n\n[dependencies]\nregia = { path = \"../e/crates/regia\" }\n\n[profile.dev.package.\"*\"]\nopt-level = 2\n";
        std::fs::write(quick.join("Cargo.toml"), manifest).expect("quick");
        std::fs::write(quick.join("src/main.rs"), "fn main() {}\n").expect("main");
        assert_eq!(plans_status(&quick), ["build.rs: add"]);
        run_with(&write_args(&quick), &[]).expect("--write");
        assert!(quick.join("build.rs").is_file());
        let text = std::fs::read_to_string(quick.join("Cargo.toml")).expect("manifest");
        let parsed: toml::Table = text.parse().expect("TOML");
        assert!(parsed["build-dependencies"].get("png").is_some());
        assert!(
            parsed["profile"].get("dev").is_some(),
            "the profile table survives"
        );
        let _ = std::fs::remove_dir_all(&quick);

        let net = old_template_project("upgrade-net-build");
        std::fs::create_dir_all(net.join("client/src")).expect("dirs");
        std::fs::write(
            net.join("client/Cargo.toml"),
            OLD_LAUNCHER.replace("g_launcher", "g_client"),
        )
        .expect("client");
        std::fs::write(net.join("client/src/main.rs"), "fn main() {}\n").expect("main");
        assert_eq!(
            plans_status(&net),
            ["launcher/build.rs: add", "client/build.rs: add"]
        );
        let _ = std::fs::remove_dir_all(&net);
    }

    #[test]
    fn a_v3_vehicle_becomes_v4_with_no_tire_transients_and_a_backup() {
        use regia::physics::vehicle_asset::{VEHICLE_FORMAT_VERSION, VehicleAsset};
        let project = regia_test_utils::unique_temp_dir("upgrade_vehicle_v3");
        let assets = project.join("assets");
        std::fs::create_dir_all(&assets).expect("assets");
        let file = assets.join("slip.vehicle.ron");
        let v3 = "(format_version: 3, name: \"slip\", config: (wheels: [(steer: true, tire_model: Slip((cornering_stiffness: 51234.5)))]))";
        std::fs::write(&file, v3).expect("write");
        run_with(
            &UpgradeProjectArgs {
                project: project.clone(),
                write: true,
                engine: None,
                json: false,
                backup: None,
            },
            &formats(),
        )
        .expect("upgrade");
        let text = std::fs::read_to_string(&file).expect("read");
        assert_eq!(VEHICLE_FORMAT_VERSION, 4);
        assert_eq!(
            regia_core::version::peek_ron_version(&text).expect("version"),
            VEHICLE_FORMAT_VERSION
        );
        let up = VehicleAsset::from_ron(&text).expect("loads");
        let tire = up.config.wheels[0].tire_model.slip().expect("slip");
        assert!(!tire.has_transients());
        assert_eq!(tire.cornering_stiffness.to_bits(), 51234.5_f32.to_bits());
        assert_eq!(Ok(up), VehicleAsset::from_ron(v3));
        let backup = assets.join("slip.vehicle.ron.bak");
        assert_eq!(std::fs::read_to_string(backup).expect("backup"), v3);
        let _ = std::fs::remove_dir_all(&project);
    }

    #[test]
    fn a_v1_vehicle_snapshot_becomes_v2_and_keeps_its_backup() {
        use regia::physics::vehicle_snapshot::VehicleSnapshot;
        let project = regia_test_utils::unique_temp_dir("upgrade_vehicle_snapshot_v1");
        let file = project.join("saved.vehicle-snapshot.ron");
        let old = r#"(format_version:1,translation:(1.0,2.0,3.0),rotation:(0.0,0.0,0.0,1.0),
            linear_velocity:(0.0,0.0,0.0),angular_velocity:(0.0,0.0,0.0),wheels:[],
            vehicle_speed:0.0,drivetrain:None,
            tires:[(anchor:None,anchor_body:None,slip:false,lateral_force:0.0,
                longitudinal_force:0.0,load:3.0,slip_angle:0.5,stuck:false)],
            input:(throttle:0.0,brake:0.0,steer:0.0),steps:7,contact_shift:(0.0,0.0,0.0),
            chassis_in_contact:false)"#;
        std::fs::write(project.join("saved.tmp"), old).expect("fixture");
        std::fs::rename(project.join("saved.tmp"), &file).expect("publish fixture");
        let before = VehicleSnapshot::from_ron(old).expect("v1 reads");
        assert_eq!(plan_with(&project, &formats()).expect("plan")[0].found, 1);
        run_with(
            &UpgradeProjectArgs {
                project: project.clone(),
                write: true,
                engine: None,
                json: false,
                backup: None,
            },
            &formats(),
        )
        .expect("upgrade");
        let text = std::fs::read_to_string(&file).expect("read");
        assert_eq!(regia_core::version::peek_ron_version(&text).expect("version"), 2);
        assert_eq!(VehicleSnapshot::from_ron(&text).expect("v2 reads"), before);
        assert_eq!(
            std::fs::read_to_string(project.join("saved.vehicle-snapshot.ron.bak")).expect("backup"),
            old
        );
        assert!(!plan_with(&project, &formats()).expect("current plan")[0].needs_migration());
        std::fs::remove_dir_all(project).expect("cleanup");
    }

    /// ADR-8121: v1/v2 `.decal.ron` upgrades to v3, preserving comments and atlas choice.
    #[test]
    fn old_decal_prefabs_upgrade_to_v3() {
        for version in [1, 2] {
            let old = format!(
                "// a scorch\n(version: {version}, name: \"s\", half_size: (1.0, 1.0, 0.3), channels: \"albedo+normal\", atlas: \"scorch\")"
            );
            let out = migrate_decal(old.as_bytes(), version).expect("migrates");
            let text = String::from_utf8(out).expect("utf-8");
            assert!(text.starts_with("// a scorch"), "{text}");
            assert!(text.contains("version: 3"), "{text}");
            let prefab = regia_vfx::DecalPrefab::from_ron(&text).expect("loads");
            assert_eq!(prefab.atlas, "scorch");
            assert!(prefab.texture.is_none());
            assert_eq!(
                migrate_decal(text.as_bytes(), 3).expect("current"),
                text.as_bytes()
            );
        }
        assert!(migrate_decal(b"(version: 9, name: \"s\", half_size: (1.0, 1.0, 1.0))", 9).is_err());
    }

    /// ADR-3237: `.fgiv` split parts a bake wrote at v4 are rewritten at v5 (with a backup), read
    /// back as the same uncropped parts, and the set still joins into the unsplit volume; a v5
    /// part beside them is current and not touched.
    #[test]
    fn upgrade_project_rewrites_v4_split_parts_at_v5() {
        use regia_asset::baked_gi::{
            BAKED_GI_VERSION, BakeInfo, BakeLights, BakedGiVolume, GI_COMPONENTS, ProbeData, ProbeFlags,
            ProbeMeta, TerrainLayer, VolumeLayout,
        };
        use regia_asset::baked_gi_split::{baked_gi_version, split_part_path};
        let dir = regia_test_utils::unique_temp_dir("upgrade-fgiv-v4");
        let mut layout = VolumeLayout {
            terrain: Some(TerrainLayer {
                origin: [0.0, 0.0],
                spacing: [2.0, 2.0],
                counts: [16, 3],
                levels: vec![1.0],
                ground: vec![0.0; 48],
            }),
            bricks: Vec::new(),
            moment_resolution: 2,
            max_distance: 8.0,
        };
        layout.assign_brick_offsets();
        let probes: Vec<ProbeData> = (0..layout.probe_count())
            .map(|i| ProbeData {
                sh: [[[0.2 + i as f32 * 0.01, 0.0, 0.0, 0.0]; 3]; GI_COMPONENTS],
                moments: vec![[1.0, 2.0]; 4],
                meta: ProbeMeta {
                    offset: [0.0; 3],
                    flags: ProbeFlags::VALID,
                },
            })
            .collect();
        let whole = BakedGiVolume::from_probes(&layout, BakeLights::default(), BakeInfo::default(), &probes)
            .expect("volume");
        let parts = whole
            .split_parts(layout.runtime_buffer_bytes() * 7 / 10)
            .expect("splits");
        assert_eq!(parts.len(), 2);
        let entry = dir.join("map.fgiv");
        let v4: Vec<Vec<u8>> = parts.iter().map(|p| p.to_bytes_v4().expect("v4")).collect();
        std::fs::write(split_part_path(&entry, 0), &v4[0]).expect("write part 0");
        std::fs::write(split_part_path(&entry, 1), parts[1].to_bytes().expect("v5")).expect("write part 1");
        let plan = plan_with(&dir, &formats()).expect("plan");
        let gi: Vec<&FilePlan> = plan.iter().filter(|p| p.format == "baked-gi").collect();
        assert_eq!(gi.len(), 2, "both part files are found");
        assert_eq!(gi.iter().filter(|p| p.needs_migration()).count(), 1);
        let args = UpgradeProjectArgs {
            project: dir.clone(),
            write: true,
            engine: None,
            json: false,
            backup: None,
        };
        run_with(&args, &formats()).expect("--write");
        let migrated = std::fs::read(&entry).expect("part 0");
        assert_eq!(baked_gi_version(&migrated).expect("version"), BAKED_GI_VERSION);
        assert_eq!(migrated, parts[0].to_bytes().expect("v5"));
        assert_eq!(
            std::fs::read(entry.with_extension("fgiv.bak")).expect("backup"),
            v4[0]
        );
        assert!(!split_part_path(&entry, 1).with_extension("fgiv.bak").exists());
        let set = BakedGiVolume::read_split_set(&entry).expect("the set reads");
        assert_eq!(set, parts);
        assert!(BakedGiVolume::join_split_set(&set).expect("joins") == whole);
        std::fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod audit_level_dir_tests {
    use super::*;

    /// A vendored engine's `.level.d` (here a broken one) is neither planned nor able to fail the
    /// project's upgrade: engine trees are never scanned (module doc, spec 94 § 4.1).
    #[test]
    fn audit_level_dirs_inside_a_vendored_engine_are_not_scanned() {
        let project = regia_test_utils::unique_temp_dir("upgrade_level_dir_engine_tree");
        let engine = project.join("engine");
        std::fs::create_dir_all(engine.join("crates/regia")).expect("engine");
        std::fs::write(
            engine.join("crates/regia/Cargo.toml"),
            "[package]\nname = \"regia\"\n",
        )
        .expect("cargo");
        std::fs::create_dir_all(engine.join("samples/x/assets/levels/a.level.d")).expect("level dir");
        assert!(distribution::is_engine_tree(&engine));
        assert!(
            plan_level_dirs(&project)
                .expect("the engine tree is skipped")
                .is_empty()
        );
        let _ = std::fs::remove_dir_all(&project);
    }

    /// `regia package` stages real frozen-format content under the project's `build/` tree and
    /// ships the packaged bundle from `dist/<platform>/` (`commands/package.rs`): both are
    /// regenerated by the next package run, so migrating them in place would rewrite shipped
    /// bytes and drop `.bak` files into the bundle (sprint-deep-zai-cli-tools finding 1).
    /// Neither walker plans them, exactly as `target` is skipped.
    #[test]
    fn regenerable_package_output_trees_are_not_planned() {
        let project = regia_test_utils::unique_temp_dir("upgrade_skip_build_dist");
        let v0 = r#"(base: (prefab: "8b6f2a11-0000-4000-8000-000000000001"))"#;
        std::fs::create_dir_all(project.join("assets")).expect("dirs");
        std::fs::write(project.join("assets/keep.variant.ron"), v0).expect("fixture");
        for tree in ["build", "dist"] {
            let staged = project.join(tree).join("linux/assets");
            std::fs::create_dir_all(&staged).expect("dirs");
            std::fs::write(staged.join("staged.variant.ron"), v0).expect("fixture");

            let level = project.join(tree).join("linux/assets/levels/yard.level.d");
            let mut scene = regia_scene::format::SceneFile::new();
            scene.entities.push(regia_scene::format::SceneEntity {
                guid: regia_core::id::Guid::from_u128(0xab),
                ..Default::default()
            });
            regia_scene::level_dir::write_level_dir(&level, &scene).expect("level dir");
        }
        // The control: the same files in the project's own assets are planned.
        let control_level = project.join("assets/levels/yard.level.d");
        let mut scene = regia_scene::format::SceneFile::new();
        scene.entities.push(regia_scene::format::SceneEntity {
            guid: regia_core::id::Guid::from_u128(0xab),
            ..Default::default()
        });
        regia_scene::level_dir::write_level_dir(&control_level, &scene).expect("level dir");

        let listed: Vec<String> = plan_with(&project, &formats())
            .expect("plan")
            .iter()
            .map(|p| p.path.display().to_string())
            .collect();
        assert_eq!(
            listed,
            ["assets/keep.variant.ron"],
            "the staged (build/) and shipped (dist/) trees are regenerated, never migrated"
        );
        let dirs: Vec<String> = plan_level_dirs(&project)
            .expect("level directories")
            .iter()
            .map(|p| p.path.display().to_string())
            .collect();
        assert_eq!(
            dirs,
            ["assets/levels/yard.level.d"],
            "level directories inside build/ and dist/ are regenerated too"
        );
        let _ = std::fs::remove_dir_all(&project);
    }

    /// A hand-written `.levelvariant.ron` without `format_version` plans as v0 and re-saves as v1
    /// (ADR-7823); a newer one is refused by the migrator.
    #[test]
    fn a_versionless_level_variant_upgrades_to_v1() {
        let dir = regia_test_utils::unique_temp_dir("upgrade-level-variant");
        let v0 = r#"(base: "asset:8b6f2a11-0000-4000-8000-000000000001", removed_entities: ["8b6f2a11-0000-4000-8000-000000000002"])"#;
        std::fs::write(dir.join("night.levelvariant.ron"), v0).expect("fixture");
        let plan = plan_with(&dir, &formats()).expect("plan");
        let found: Vec<&FilePlan> = plan.iter().filter(|p| p.format == "level-variant").collect();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].found, 0);
        assert!(found[0].needs_migration());
        let out = migrate_level_variant(v0.as_bytes(), 0).expect("migrates");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.starts_with("LevelVariant("), "{text}");
        assert!(text.contains("format_version: 1"), "{text}");
        assert!(migrate_level_variant(b"(format_version: 9, base: \"x\")", 9).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod backup_tests {
    use super::*;

    fn project_with_sources(label: &str) -> PathBuf {
        let dir = regia_test_utils::unique_temp_dir(label);
        for (rel, text) in [
            ("Regia.toml", "[project]\nname = \"G\"\nid = \"com.example.g\"\n"),
            ("assets/maps/main.level.ron", "( entities: [] )"),
            ("src/lib.rs", "// game\n"),
            ("target/debug/game.so", "binary"),
            ("dist/game.pak", "pak"),
            (".git/HEAD", "ref"),
            (".regia/cache/blob", "cache"),
            ("tools/helper/target/CACHEDIR.TAG", "tag"),
            ("tools/helper/src/main.rs", "fn main() {}\n"),
        ] {
            let path = dir.join(rel);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
            std::fs::write(&path, text).expect("file");
        }
        dir
    }

    fn files_under(root: &Path) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("read").flatten() {
                if entry.path().is_dir() {
                    stack.push(entry.path());
                } else {
                    out.push(distribution::slash_path(
                        entry.path().strip_prefix(root).expect("inside"),
                    ));
                }
            }
        }
        out.sort();
        out
    }

    fn args(project: &Path, write: bool, backup: Option<PathBuf>) -> UpgradeProjectArgs {
        UpgradeProjectArgs {
            project: project.to_path_buf(),
            write,
            engine: None,
            json: false,
            backup,
        }
    }

    /// P-327: `--backup <dir>` with `--write` copies the project's source files (no target/,
    /// dist/, .git/, `.regia/` subfolders or cache folders) to a dated folder before anything is
    /// migrated.
    #[test]
    fn backup_copies_the_project_sources_before_write() {
        let project = project_with_sources("upgrade_backup_project");
        let backups = regia_test_utils::unique_temp_dir("upgrade_backup_dir");
        run_with(&args(&project, true, Some(backups.clone())), &formats()).expect("upgrade");
        let made: Vec<PathBuf> = std::fs::read_dir(&backups)
            .expect("backups")
            .flatten()
            .map(|e| e.path())
            .collect();
        assert_eq!(made.len(), 1, "one dated folder, no staging left: {made:?}");
        let name = made[0]
            .file_name()
            .and_then(|n| n.to_str())
            .expect("name")
            .to_string();
        let project_name = project
            .file_name()
            .and_then(|n| n.to_str())
            .expect("project name");
        assert!(name.starts_with(&format!("{project_name}-backup-")), "{name}");
        assert_eq!(
            files_under(&made[0]),
            [
                "Regia.toml",
                "assets/maps/main.level.ron",
                "src/lib.rs",
                "tools/helper/src/main.rs"
            ]
        );
        // A second run on the same second gets its own folder.
        let report = backup_project(&project, &backups, 0).expect("backup");
        let again = backup_project(&project, &backups, 0).expect("backup again");
        assert_ne!(report.path, again.path);
        assert!(
            again
                .path
                .to_string_lossy()
                .ends_with("-backup-19700101-000000-2"),
            "{}",
            again.path.display()
        );
        assert!(report.skipped.contains(&"target/".to_string()), "{report:?}");
        assert!(
            report.skipped.contains(&"tools/helper/target/".to_string()),
            "{report:?}"
        );
        std::fs::remove_dir_all(&project).ok();
        std::fs::remove_dir_all(&backups).ok();
    }

    /// A dry run copies nothing; a backup folder inside the project is refused before anything is
    /// written.
    #[test]
    fn backup_is_written_only_by_write_and_never_inside_the_project() {
        let project = project_with_sources("upgrade_backup_refuse");
        let backups = regia_test_utils::unique_temp_dir("upgrade_backup_dry");
        run_with(&args(&project, false, Some(backups.clone())), &formats()).expect("dry run");
        assert_eq!(
            std::fs::read_dir(&backups).expect("dir").count(),
            0,
            "a dry run copies nothing"
        );
        let inside = project.join("backups");
        let err = run_with(&args(&project, true, Some(inside.clone())), &formats())
            .expect_err("inside the project");
        assert!(err.to_string().starts_with("backup_inside_project"), "{err}");
        assert_eq!(std::fs::read_dir(&inside).expect("dir").count(), 0);
        std::fs::remove_dir_all(&project).ok();
        std::fs::remove_dir_all(&backups).ok();
    }

    /// Review A2 (ADR-8435): `--write` migrates the project-local editor files in the state
    /// directory (`.regia/editor_layout.ron` v1 -> current), so the backup holds their originals,
    /// byte for byte; the state directory's subfolders (caches, cooked output) stay out.
    #[test]
    fn backup_keeps_the_state_dir_files_write_migrates() {
        let state = regia_core::brand::PROJECT_STATE_DIR;
        let project = project_with_sources("upgrade_backup_state_files");
        let layout_v1 = "(\n    format_version: 1,\n    root: Tabs(tabs: [\"viewport\"], active: 0),\n)\n";
        let layout = project.join(state).join("editor_layout.ron");
        std::fs::write(&layout, layout_v1).expect("layout");
        std::fs::write(project.join(state).join("agent.toml"), "read_only = true\n").expect("agent.toml");
        let backups = regia_test_utils::unique_temp_dir("upgrade_backup_state_dir");
        run_with(&args(&project, true, Some(backups.clone())), &formats()).expect("upgrade");
        let migrated = std::fs::read_to_string(&layout).expect("migrated layout");
        // The current version, not a literal: the panel-instance bump (ADR-8105) moved it to v3 (ADR-8580).
        let current = regia_core::editor_formats::editor_format("editor-layout")
            .expect("registered")
            .version;
        assert!(
            migrated.contains(&format!("format_version: {current}")),
            "{migrated}"
        );
        let made: Vec<PathBuf> = std::fs::read_dir(&backups)
            .expect("backups")
            .flatten()
            .map(|e| e.path())
            .collect();
        assert_eq!(made.len(), 1, "{made:?}");
        let kept = std::fs::read_to_string(made[0].join(state).join("editor_layout.ron"))
            .expect("the original layout is in the backup");
        assert_eq!(kept, layout_v1, "the backup holds the pre-migration bytes");
        assert_eq!(
            files_under(&made[0]),
            [
                format!("{state}/agent.toml"),
                format!("{state}/editor_layout.ron"),
                "Regia.toml".to_string(),
                "assets/maps/main.level.ron".to_string(),
                "src/lib.rs".to_string(),
                "tools/helper/src/main.rs".to_string(),
            ],
            "the state directory's files are kept, its cache/ subfolder is not"
        );
        std::fs::remove_dir_all(&project).ok();
        std::fs::remove_dir_all(&backups).ok();
    }

    /// Review A2 (ADR-8435): a project still on the legacy state directory name (`.ferrite/`)
    /// gets its editor files backed up under that name before `--write` migrates them and moves
    /// the directory to the current name; the legacy directory's subfolders stay out.
    #[test]
    fn backup_keeps_the_legacy_state_dir_files_before_the_brand_move() {
        let legacy = regia_core::brand::LEGACY_PROJECT_STATE_DIR;
        let current = regia_core::brand::PROJECT_STATE_DIR;
        let project = regia_test_utils::unique_temp_dir("upgrade_backup_legacy_state");
        let layout_v1 = "(format_version: 1, root: Tabs(tabs: [\"outliner\"], active: 0))\n";
        for (rel, text) in [
            ("Regia.toml", "[project]\nname = \"G\"\nid = \"com.example.g\"\n"),
            (&format!("{legacy}/editor_layout.ron") as &str, layout_v1),
            (&format!("{legacy}/cooked/blob") as &str, "cooked"),
        ] {
            let path = project.join(rel);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
            std::fs::write(&path, text).expect("file");
        }
        let backups = regia_test_utils::unique_temp_dir("upgrade_backup_legacy_dir");
        run_with(&args(&project, true, Some(backups.clone())), &formats()).expect("upgrade");
        if legacy != current {
            assert!(project.join(current).join("editor_layout.ron").is_file(), "moved");
        }
        let made: Vec<PathBuf> = std::fs::read_dir(&backups)
            .expect("backups")
            .flatten()
            .map(|e| e.path())
            .collect();
        assert_eq!(made.len(), 1, "{made:?}");
        let kept = std::fs::read_to_string(made[0].join(legacy).join("editor_layout.ron"))
            .expect("the legacy layout is in the backup");
        assert_eq!(kept, layout_v1);
        assert!(
            !made[0].join(legacy).join("cooked").exists(),
            "cooked output stays out"
        );
        std::fs::remove_dir_all(&project).ok();
        std::fs::remove_dir_all(&backups).ok();
    }

    /// The flag parses as `--backup DIR`.
    #[test]
    fn the_backup_flag_parses() {
        #[derive(clap::Parser)]
        struct Cli {
            #[command(flatten)]
            args: UpgradeProjectArgs,
        }
        let cli =
            <Cli as clap::Parser>::try_parse_from(["regia", "--write", "--backup", "../bk"]).expect("parses");
        assert_eq!(cli.args.backup, Some(PathBuf::from("../bk")));
        assert!(cli.args.write);
        assert_eq!(
            backup_folder_name(Path::new("/nonexistent/My Game"), 86_400 + 3_661),
            "project-backup-19700102-010101",
            "an unreadable project is named `project`"
        );
    }

    /// Format batch 2 (ADR-10064, ADR-7724 one-step rule): a `.playlist.ron` v1 plans as needing
    /// the v1 -> v2 step, `--write` rewrites only its version number, a second `--write` changes no
    /// byte, and a file patched to v3 plans as newer (never downgraded).
    #[test]
    fn playlist_ron_v1_upgrades_to_v2_and_write_twice_is_idempotent() {
        let v1 = include_str!("../../../regia_agent/tests/fixtures/format-corpus/playlist-ron/v1.ron");
        let v2 = v1.replacen("format_version: 1", "format_version: 2", 1);
        let project = project_with_sources("upgrade-playlist-v2");
        let file = project.join("assets/server/rows.playlist.ron");
        std::fs::create_dir_all(file.parent().expect("parent")).expect("folder");
        std::fs::write(&file, v1).expect("v1 playlist");
        let plan = plan_with(&project, &formats()).expect("plan");
        let found = plan.iter().find(|p| p.format == "playlist-ron").expect("planned");
        assert_eq!(found.found, 1);
        assert!(found.needs_migration(), "v1 takes the v1 -> v2 step");
        for _ in 0..2 {
            run_with(&args(&project, true, None), &formats()).expect("upgrade --write");
            assert_eq!(
                std::fs::read_to_string(&file).expect("playlist"),
                v2,
                "only the version number moves"
            );
        }
        let plan = plan_with(&project, &formats()).expect("plan");
        let found = plan.iter().find(|p| p.format == "playlist-ron").expect("planned");
        assert_eq!(found.found, 2);
        assert!(!found.needs_migration(), "v2 is current");
        std::fs::write(&file, v1.replacen("format_version: 1", "format_version: 3", 1)).expect("v3");
        let plan = plan_with(&project, &formats()).expect("plan");
        let newer = plan.iter().find(|p| p.format == "playlist-ron").expect("planned");
        assert_eq!(newer.found, 3);
        assert!(newer.status().contains("newer"), "{}", newer.status());
        std::fs::remove_dir_all(&project).ok();
    }

    /// Format batch 2 (ADR-8880, ADR-7724 one-step rule): a `match-result` v1 record in a history
    /// folder plans as current, `--write` twice leaves it byte for byte, a record patched to v2 plans
    /// as newer (never downgraded) and other JSON in the folder is not taken for a record.
    #[test]
    fn match_result_records_plan_as_current_and_write_twice_is_idempotent() {
        let v1 = include_str!("../../../regia_agent/tests/fixtures/format-corpus/match-result/v1.json");
        let project = project_with_sources("upgrade-match-result");
        let day = project.join("server/matches/2026-11-18");
        std::fs::create_dir_all(&day).expect("day folder");
        let record = day.join("6d617463-682d-7265-7375-6c7400000001.json");
        std::fs::write(&record, v1).expect("record");
        std::fs::write(day.join("notes.json"), r#"{"format":"other","version":9}"#).expect("other json");
        let plan = plan_with(&project, &formats()).expect("plan");
        let found: Vec<&FilePlan> = plan.iter().filter(|p| p.format == "match-result").collect();
        assert_eq!(
            found.len(),
            1,
            "only the record is a match-result: {:?}",
            found.iter().map(|p| &p.path).collect::<Vec<_>>()
        );
        assert_eq!(found[0].found, 1);
        assert!(!found[0].needs_migration(), "v1 is the current version");
        for _ in 0..2 {
            run_with(&args(&project, true, None), &formats()).expect("upgrade --write");
            assert_eq!(
                std::fs::read_to_string(&record).expect("record"),
                v1,
                "left byte for byte"
            );
        }
        std::fs::write(&record, v1.replacen("\"version\": 1,", "\"version\": 2,", 1)).expect("v2");
        let plan = plan_with(&project, &formats()).expect("plan");
        let newer = plan.iter().find(|p| p.format == "match-result").expect("planned");
        assert_eq!(newer.found, 2);
        assert!(newer.status().contains("newer"), "{}", newer.status());
        assert!(
            regia_framework::match_results::migrate_match_result(v1.as_bytes(), 1).is_err(),
            "no migrator from v1 exists while v1 is current"
        );
        std::fs::remove_dir_all(&project).ok();
    }
}

#[cfg(test)]
mod terrain_index_upgrade_tests {
    use super::*;
    use regia_asset::{AssetId, COOKED_FORMAT_VERSION, CookedHeader, encode_cooked_file};
    use regia_terrain::stream::{
        TERRAIN_INDEX_FORMAT_VERSION, TERRAIN_INDEX_TYPE_PATH, TerrainHeightfieldIndex, TerrainIndex,
        load_terrain_index_bytes, terrain_index_file_version,
    };

    fn cooked(payload: &[u8]) -> Vec<u8> {
        let header = CookedHeader {
            format_version: COOKED_FORMAT_VERSION,
            type_tag: regia_core::id::TypePathHash::of(TERRAIN_INDEX_TYPE_PATH),
            asset_id: AssetId::sub_asset(AssetId::NIL, "terrain/index"),
            importer_version: 1,
            dependencies: Vec::new(),
            payload_len: payload.len() as u64,
        };
        encode_cooked_file(&header, payload).expect("encode")
    }

    fn v1_index() -> TerrainIndex {
        TerrainIndex {
            version: 1,
            map_id: AssetId::sub_asset(AssetId::NIL, "map"),
            slots: Vec::new(),
            heightfield: Some(TerrainHeightfieldIndex {
                width: 5,
                height: 5,
                tile_size: 256,
                tiles_x: 1,
                tiles_y: 1,
                bytes: 80,
            }),
            layers: vec!["grass".into()],
            world_size_m: Some(64.0),
            world_origin: Some([1.0, 2.0, 3.0]),
            heights: None,
            holes: None,
            material: None,
            source_mapping: None,
            source_spacing_z_m: None,
            source_valid_mask: None,
        }
    }

    /// ADR-8471: a v1 cooked terrain index plans as `terrain-index` v1 and migrates to v2 with no
    /// mapping, every other value kept; a broken index in the same run stops the run before any
    /// file is written, so both originals stay byte for byte.
    #[test]
    fn write_upgrades_a_v1_terrain_index_and_an_interrupted_run_keeps_the_originals() {
        let dir = regia_test_utils::unique_temp_dir("upgrade-terrain-index");
        let good = dir.join("index.fckd");
        let original = cooked(&v1_index().to_postcard().expect("v1 payload"));
        std::fs::write(&good, &original).expect("write v1");
        assert_eq!(terrain_index_file_version(&original).expect("probe"), Some(1));
        // A v1 payload cut short: the probe reads its version, the migrator cannot decode it.
        let broken = dir.join("broken.fckd");
        let cut = cooked(&v1_index().to_postcard().expect("v1 payload")[..6]);
        std::fs::write(&broken, &cut).expect("write broken");

        let plan = plan_with(&dir, &formats()).expect("plan");
        let found: Vec<&FilePlan> = plan.iter().filter(|p| p.format == "terrain-index").collect();
        assert_eq!(found.len(), 2, "both indexes are planned");
        assert!(found.iter().all(|p| p.found == 1 && p.needs_migration()));

        let args = UpgradeProjectArgs {
            project: dir.clone(),
            write: true,
            engine: None,
            json: false,
            backup: None,
        };
        assert!(
            run_with(&args, &formats()).is_err(),
            "the broken index stops the run"
        );
        assert_eq!(std::fs::read(&good).expect("good"), original, "nothing written");
        assert_eq!(std::fs::read(&broken).expect("broken"), cut);
        assert!(!dir.join("index.fckd.bak").exists());

        std::fs::remove_file(&broken).expect("remove broken");
        run_with(&args, &formats()).expect("--write migrates the v1 index");
        let migrated = std::fs::read(&good).expect("migrated");
        assert_eq!(
            terrain_index_file_version(&migrated).expect("probe"),
            Some(TERRAIN_INDEX_FORMAT_VERSION)
        );
        let back = load_terrain_index_bytes(&migrated).expect("v2 loads");
        assert_eq!(
            back,
            TerrainIndex {
                version: TERRAIN_INDEX_FORMAT_VERSION,
                ..v1_index()
            }
        );
        assert_eq!(
            std::fs::read(dir.join("index.fckd.bak")).expect("backup"),
            original
        );
        let again = plan_with(&dir, &formats()).expect("plan");
        assert!(
            again
                .iter()
                .filter(|p| p.format == "terrain-index")
                .all(|p| !p.needs_migration()),
            "the migrated index is current"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ADR-8871: a v0.6.0 (v2) cooked terrain index with a count mapping is planned as v2 and
    /// `--write` makes it v3 with no Z spacing or mask, every other value kept; a second `--write`
    /// leaves the same bytes (idempotent).
    #[test]
    fn write_upgrades_a_v2_terrain_index_to_v3_and_twice_gives_the_same_bytes() {
        let dir = regia_test_utils::unique_temp_dir("upgrade-terrain-index-v2");
        let path = dir.join("index.fckd");
        let v2 = TerrainIndex {
            version: 2,
            source_mapping: Some(regia_terrain::source::CountMapping {
                cols: 5,
                rows: 5,
                meters_per_count: 0.02,
                zero_counts: 500,
                cell_m: 4.0,
                sample0_m: [-8.0, -8.0],
            }),
            ..v1_index()
        };
        let original = cooked(&v2.to_postcard().expect("v2 payload"));
        std::fs::write(&path, &original).expect("write v2");
        assert_eq!(terrain_index_file_version(&original).expect("probe"), Some(2));
        let plan = plan_with(&dir, &formats()).expect("plan");
        let found: Vec<&FilePlan> = plan.iter().filter(|p| p.format == "terrain-index").collect();
        assert_eq!(found.len(), 1);
        assert!(found[0].found == 2 && found[0].needs_migration());

        let args = UpgradeProjectArgs {
            project: dir.clone(),
            write: true,
            engine: None,
            json: false,
            backup: None,
        };
        run_with(&args, &formats()).expect("--write migrates the v2 index");
        let once = std::fs::read(&path).expect("migrated");
        assert_eq!(terrain_index_file_version(&once).expect("probe"), Some(3));
        let back = load_terrain_index_bytes(&once).expect("v3 loads");
        assert_eq!(
            back,
            TerrainIndex {
                version: TERRAIN_INDEX_FORMAT_VERSION,
                ..v2
            }
        );
        run_with(&args, &formats()).expect("a second --write");
        assert_eq!(std::fs::read(&path).expect("twice"), once, "idempotent");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
