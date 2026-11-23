// SPDX-License-Identifier: MPL-2.0
//! Cook of gameplay data tables (spec 51 § 4.4, spec 39 § 3, ADR-2012): `.curves.ron`,
//! `.curves.csv`, `.chooser.ron`, `.smartobject.ron` and `.camera.ron` (ADR-2030), and the typed data
//! tables `.table.ron` / `.table.csv` (`docs/specs/content-data.md` § 3, ADR-2132), which cook to
//! [`regia_asset::data_table::CookedDataTable`] after their structural checks (the row struct is the
//! game's, so typed checks run when the game loads them). They ship raw too, so one that fails its checks
//! is left out of the pak with a warning rather than failing the cook.
//!
//! A table is validated like a load would validate it, and a table with errors fails the cook
//! (warnings do not). Curve tables and choosers are stored in their postcard form; a smart object
//! definition and a camera asset keep their canonical RON text, which only needs a pak entry and an
//! [`AssetId`].
//! Choosers also pass the checks that need other tables (nested references, cycles, depth), so a
//! chooser that cooks does not fail on them in the game.
//!
//! Net profiles (`.netprofile.ron`) and custom data assets (`.data.ron`) cook here too (ADR-7232):
//! both keep their RON text (a profile canonicalized after its checks, a data asset as written,
//! since its value type is the game's).
//!
//! `cook_project` cooks these after every other source: the [`AssetId`]s a chooser records as
//! dependencies come from the `.meta` of the assets it names, and a first cook only has them then.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use regia_ai::smart_object_def::{SMART_OBJECT_DEF_TYPE_PATH, SmartObjectDef};
use regia_asset::data_table::{CookedDataTable, DATA_TABLE_COOKED_TYPE_PATH, TableFormat};
use regia_asset::{AssetId, MetaSidecar, meta_path_for, normalise_asset_path};
use regia_framework::abilities::{
    ABILITIES_COOKED_TYPE_PATH, ABILITY_FORMAT_VERSION, AbilityAsset, cook_ability_set, parse_ron,
};
use regia_framework::camera::asset::{CAMERA_ASSET_TYPE_PATH, CameraAsset, Severity};
use regia_framework::chooser::{CHOOSER_TYPE_PATH, ChooserTable};
use regia_framework::curve_table::{CURVE_TABLE_TYPE_PATH, parse_curve_table};
use regia_ui::anim_pack::UIANIM_TYPE_PATH;
use regia_ui::anim_timeline::UiAnimLibrary;

use crate::cook::{CookError, input_files_hash, join_in_project, name_ends_with};
use crate::db::AssetDatabase;

/// What cooking one table produced.
pub(crate) struct CookedTable {
    pub(crate) importer: String,
    pub(crate) settings_hash: [u8; 32],
    pub(crate) id: AssetId,
    pub(crate) type_path: String,
    pub(crate) payload: Vec<u8>,
    pub(crate) dependencies: Vec<AssetId>,
}

/// The importer name of a data table's file name, `None` for any other file.
fn importer_of(name: &str) -> Option<&'static str> {
    if name_ends_with(name, ".curves.ron") || name_ends_with(name, ".curves.csv") {
        Some("curve_table")
    } else if name_ends_with(name, ".chooser.ron") {
        Some("chooser")
    } else if name_ends_with(name, ".smartobject.ron") {
        Some("smartobject")
    } else if name_ends_with(name, ".camera.ron") {
        Some("camera")
    } else if name_ends_with(name, ".uianim.ron") {
        Some("uianim")
    } else if name_ends_with(name, ".table.ron") || name_ends_with(name, ".table.csv") {
        Some("data_table")
    } else if regia_framework::abilities::is_ability_asset_name(name) {
        // Case-sensitive, as the runtime's `AssetKind::of` is: a mixed-case name would cook but
        // never load (ADR-9370).
        Some("abilities")
    } else if name_ends_with(name, ".netprofile.ron") {
        Some("netprofile")
    } else if name_ends_with(name, regia_asset::data_asset::DATA_ASSET_SUFFIX) {
        Some("data_asset")
    } else if name_ends_with(name, regia_ai::det::DET_UTILITY_SUFFIX) {
        Some("det_utility")
    } else {
        None
    }
}

/// Whether the file name `name` is a data table this module cooks (ignoring ASCII case).
#[must_use]
pub fn is_data_table_name(name: &str) -> bool {
    importer_of(name).is_some()
}

/// Whether `path` is a typed data table (`.table.ron`, `.table.csv`): one that ships raw as well, so a
/// failed cook is a warning (ADR-2132).
#[must_use]
pub fn is_typed_data_table(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| importer_of(n) == Some("data_table"))
}

/// Whether `path` is a data table this module cooks.
#[must_use]
pub fn is_data_table(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(is_data_table_name)
}

fn table_error(message: impl Into<String>) -> CookError {
    CookError::DataTable(message.into())
}

fn text_of(bytes: &[u8]) -> Result<&str, CookError> {
    std::str::from_utf8(bytes).map_err(|e| table_error(format!("not UTF-8: {e}")))
}

/// `path` (an asset path, `#sub-asset` dropped) as a file of `project_root`, if it exists.
fn asset_file(project_root: &Path, path: &str) -> Option<PathBuf> {
    let (source, _) = path.split_once('#').unwrap_or((path, ""));
    join_in_project(project_root, &format!("assets/{source}"))
        .ok()
        .filter(|file| file.is_file())
}

/// The id of the asset at `path`, or `None` when nothing cooks it: a raw file ships as itself, so
/// a pak has no entry to depend on. A data table not cooked yet gets its `.meta` here, since
/// tables may name each other in any order.
fn reference_id(project_root: &Path, path: &str) -> Option<AssetId> {
    let file = asset_file(project_root, path)?;
    let name = file.file_name()?.to_str()?;
    if let Some(importer) = importer_of(name) {
        return AssetDatabase::load_or_create_meta(&file, importer)
            .ok()
            .map(|m| m.id);
    }
    let bytes = std::fs::read(meta_path_for(&file)).ok()?;
    MetaSidecar::from_ron(&bytes).ok().map(|m| m.id)
}

/// `chooser`'s nested tables and theirs, by asset path, read from `project_root`. A file that is
/// missing or does not load stays out: `validate_at` then reports `chooser_ref_unresolved`, and
/// the file's own cook reports what is wrong with it.
fn nested_tables(
    project_root: &Path,
    chooser: &ChooserTable,
) -> (BTreeMap<String, ChooserTable>, Vec<PathBuf>) {
    let mut tables = BTreeMap::new();
    let mut files = Vec::new();
    let mut seen = BTreeSet::new();
    let mut todo = chooser.nested_choosers();
    while let Some(path) = todo.pop() {
        if !seen.insert(path.clone()) {
            continue;
        }
        let Some(file) = asset_file(project_root, &path) else {
            continue;
        };
        let loaded = std::fs::read_to_string(&file)
            .ok()
            .and_then(|text| ChooserTable::from_ron_str(&text).ok());
        files.push(file);
        if let Some(table) = loaded {
            todo.extend(table.nested_choosers());
            tables.insert(path, table);
        }
    }
    (tables, files)
}

fn cook_curve_table(source: &Path, name: &str, bytes: &[u8]) -> Result<CookedTable, CookError> {
    let meta = AssetDatabase::load_or_create_meta(source, "curve_table").map_err(CookError::Db)?;
    let table = parse_curve_table(&name.to_ascii_lowercase(), text_of(bytes)?)
        .map_err(|e| table_error(e.to_string()))?;
    Ok(CookedTable {
        importer: "curve_table".to_owned(),
        settings_hash: *blake3::hash(bytes).as_bytes(),
        id: meta.id,
        type_path: CURVE_TABLE_TYPE_PATH.to_owned(),
        payload: table.to_cooked().map_err(|e| table_error(e.to_string()))?,
        dependencies: Vec::new(),
    })
}

fn cook_chooser(source: &Path, bytes: &[u8], project_root: &Path) -> Result<CookedTable, CookError> {
    let meta = AssetDatabase::load_or_create_meta(source, "chooser").map_err(CookError::Db)?;
    let table = ChooserTable::from_ron_str(text_of(bytes)?).map_err(|e| table_error(e.to_string()))?;
    let (nested, files) = nested_tables(project_root, &table);
    let at = source
        .strip_prefix(project_root.join("assets"))
        .map(normalise_asset_path)
        .ok();
    let issues = table.validate_at(at.as_deref(), &nested);
    let errors: Vec<String> = issues
        .iter()
        .filter(|issue| issue.is_error())
        .map(ToString::to_string)
        .collect();
    if !errors.is_empty() {
        return Err(table_error(errors.join("; ")));
    }
    let mut dependencies: Vec<AssetId> = table
        .asset_references()
        .iter()
        .filter_map(|path| reference_id(project_root, path))
        .filter(|id| *id != meta.id)
        .collect();
    dependencies.sort();
    dependencies.dedup();
    // The nested files' bytes decide the checks above and the ids decide the header, so either
    // changing must rewrite this cook even when this file did not change.
    let mut key = blake3::Hasher::new();
    key.update(&input_files_hash(bytes, files));
    for id in &dependencies {
        key.update(id.to_string().as_bytes());
    }
    Ok(CookedTable {
        importer: "chooser".to_owned(),
        settings_hash: *key.finalize().as_bytes(),
        id: meta.id,
        type_path: CHOOSER_TYPE_PATH.to_owned(),
        payload: table.to_cooked().map_err(|e| table_error(e.to_string()))?,
        dependencies,
    })
}

fn cook_smart_object(source: &Path, bytes: &[u8]) -> Result<CookedTable, CookError> {
    let meta = AssetDatabase::load_or_create_meta(source, "smartobject").map_err(CookError::Db)?;
    let def = SmartObjectDef::from_ron(text_of(bytes)?).map_err(|e| table_error(e.to_string()))?;
    let text = def.to_ron().map_err(|e| table_error(e.to_string()))?;
    Ok(CookedTable {
        importer: "smartobject".to_owned(),
        settings_hash: *blake3::hash(bytes).as_bytes(),
        id: meta.id,
        type_path: SMART_OBJECT_DEF_TYPE_PATH.to_owned(),
        payload: text.into_bytes(),
        dependencies: Vec::new(),
    })
}

/// A camera asset stays RON text. Director hooks are the game's code, so the cook cannot know their
/// names and does not check them; the load in the game does.
fn cook_camera(source: &Path, bytes: &[u8]) -> Result<CookedTable, CookError> {
    let meta = AssetDatabase::load_or_create_meta(source, "camera").map_err(CookError::Db)?;
    let asset = CameraAsset::from_ron(text_of(bytes)?).map_err(|e| table_error(e.to_string()))?;
    let issues = asset.validate(&|_| true);
    if let Some(issue) = issues.iter().find(|issue| issue.severity == Severity::Error) {
        return Err(table_error(format!("{}: {}", issue.code, issue.message)));
    }
    let text = asset.to_ron().map_err(|e| table_error(e.to_string()))?;
    Ok(CookedTable {
        importer: "camera".to_owned(),
        settings_hash: *blake3::hash(bytes).as_bytes(),
        id: meta.id,
        type_path: CAMERA_ASSET_TYPE_PATH.to_owned(),
        payload: text.into_bytes(),
        dependencies: Vec::new(),
    })
}

/// A UI animation library (`.uianim.ron`, ADR-3092) stays canonical RON text; a malformed library fails
/// the cook with the library's own error (`AnimError`).
fn cook_uianim(source: &Path, bytes: &[u8]) -> Result<CookedTable, CookError> {
    let meta = AssetDatabase::load_or_create_meta(source, "uianim").map_err(CookError::Db)?;
    let lib = UiAnimLibrary::from_ron(text_of(bytes)?).map_err(|e| table_error(e.to_string()))?;
    let text = lib.to_ron_string().map_err(|e| table_error(e.to_string()))?;
    Ok(CookedTable {
        importer: "uianim".to_owned(),
        settings_hash: *blake3::hash(bytes).as_bytes(),
        id: meta.id,
        type_path: UIANIM_TYPE_PATH.to_owned(),
        payload: text.into_bytes(),
        dependencies: Vec::new(),
    })
}

/// A typed data table: structural checks, then its cooked form. Errors fail the cook; warnings (a row
/// without a column other rows have, which its field default may fill) do not.
fn cook_data_table(source: &Path, name: &str, bytes: &[u8]) -> Result<CookedTable, CookError> {
    let meta = AssetDatabase::load_or_create_meta(source, "data_table").map_err(CookError::Db)?;
    let format = TableFormat::of_path(name);
    let table = CookedDataTable::from_text(text_of(bytes)?, format).map_err(|issues| {
        let errors: Vec<String> = issues.iter().map(ToString::to_string).collect();
        table_error(errors.join("; "))
    })?;
    Ok(CookedTable {
        importer: "data_table".to_owned(),
        settings_hash: *blake3::hash(bytes).as_bytes(),
        id: meta.id,
        type_path: DATA_TABLE_COOKED_TYPE_PATH.to_owned(),
        payload: table.to_bytes().map_err(table_error)?,
        dependencies: Vec::new(),
    })
}

/// The cost and cooldown paths an `.ability.ron` text names (none for another format, or for a
/// file that does not parse: its own cook reports that).
fn ability_references(path: &str, text: &str) -> Vec<String> {
    if !path.ends_with(".ability.ron") {
        return Vec::new();
    }
    parse_ron::<AbilityAsset>(text, ABILITY_FORMAT_VERSION)
        .map(|a| a.cost.into_iter().chain(a.cooldown).collect())
        .unwrap_or_default()
}

/// An ability format file (`.attrs.ron`, `.effect.ron`, `.ability.ron`; spec 53 § 4.1, ADR-9370):
/// the file and every file its cost and cooldown references reach are cooked as one set in load
/// order ([`cook_ability_set`]), so an ability validates against its cost and cooldown as the game's
/// load does, and a reference to a missing file (`abilities_reference_missing`) or a loop of
/// references (`abilities_reference_cycle`) fails the cook. The referenced files' bytes are part of
/// the cache key, so editing a cost effect re-checks the abilities naming it.
fn cook_ability_asset(source: &Path, bytes: &[u8], project_root: &Path) -> Result<CookedTable, CookError> {
    let meta = AssetDatabase::load_or_create_meta(source, "abilities").map_err(CookError::Db)?;
    let own = source
        .strip_prefix(project_root.join("assets"))
        .map(normalise_asset_path)
        .map_err(|_| table_error(format!("{} is not under assets/", source.display())))?;
    let mut files = BTreeMap::new();
    let mut read = Vec::new();
    let mut todo = vec![(own.clone(), text_of(bytes)?.to_owned())];
    let mut seen = BTreeSet::new();
    while let Some((path, text)) = todo.pop() {
        if !seen.insert(path.clone()) {
            continue;
        }
        for reference in ability_references(&path, &text) {
            if seen.contains(&reference) {
                continue;
            }
            if let Some(file) = asset_file(project_root, &reference)
                && let Ok(text) = std::fs::read_to_string(&file)
            {
                read.push(file);
                todo.push((reference, text));
            }
        }
        files.insert(path, text);
    }
    let cooked = cook_ability_set(&files, None).map_err(|errors| {
        let mine: Vec<String> = errors
            .iter()
            .filter(|(path, _)| *path == own)
            .map(|(_, d)| format!("{}: {}", d.code, d.message))
            .collect();
        if mine.is_empty() {
            let others: Vec<String> = errors
                .iter()
                .map(|(path, d)| format!("`{path}`: {}: {}", d.code, d.message))
                .collect();
            table_error(format!(
                "abilities_reference_invalid: a file `{own}` names does not cook: {}",
                others.join("; ")
            ))
        } else {
            table_error(mine.join("; "))
        }
    })?;
    let payload = cooked
        .get(&own)
        .cloned()
        .ok_or_else(|| table_error(format!("`{own}` produced no cooked asset")))?;
    let mut dependencies: Vec<AssetId> = files
        .keys()
        .filter(|path| **path != own)
        .filter_map(|path| reference_id(project_root, path))
        .filter(|id| *id != meta.id)
        .collect();
    dependencies.sort();
    dependencies.dedup();
    Ok(CookedTable {
        importer: "abilities".to_owned(),
        settings_hash: input_files_hash(bytes, read),
        id: meta.id,
        type_path: ABILITIES_COOKED_TYPE_PATH.to_owned(),
        payload,
        dependencies,
    })
}

/// A net profile (`.netprofile.ron`, spec 72 § 8, ADR-7232) is validated like a load validates it
/// and stays canonical RON text, so `regia_net` reads it through the asset server.
fn cook_net_profile(source: &Path, bytes: &[u8]) -> Result<CookedTable, CookError> {
    let meta = AssetDatabase::load_or_create_meta(source, "netprofile").map_err(CookError::Db)?;
    let profile = regia_net::NetProfile::from_ron(text_of(bytes)?).map_err(|e| table_error(e.to_string()))?;
    profile.validate().map_err(|e| table_error(e.to_string()))?;
    let text = profile.to_ron().map_err(|e| table_error(e.to_string()))?;
    Ok(CookedTable {
        importer: "netprofile".to_owned(),
        settings_hash: *blake3::hash(bytes).as_bytes(),
        id: meta.id,
        type_path: regia_net::profile_assets::NET_PROFILE_TYPE_PATH.to_owned(),
        payload: text.into_bytes(),
        dependencies: Vec::new(),
    })
}

/// A deterministic utility catalogue (`.detutility.ron` v1, gap AI-7) is validated like a load validates it
/// (unknown inputs, duplicate names, non-finite floats, caps, a newer version) and stored as its canonical
/// text, which `regia_detsim::ai::UtilityCatalogue::from_ron_str` reads back with no asset system.
fn cook_det_utility(source: &Path, bytes: &[u8]) -> Result<CookedTable, CookError> {
    let meta = AssetDatabase::load_or_create_meta(source, "det_utility").map_err(CookError::Db)?;
    let catalogue = regia_ai::det::UtilityCatalogue::from_ron_str(text_of(bytes)?)
        .map_err(|e| table_error(format!("{}: {e}", e.code())))?;
    Ok(CookedTable {
        importer: "det_utility".to_owned(),
        settings_hash: *blake3::hash(bytes).as_bytes(),
        id: meta.id,
        type_path: regia_ai::det::DET_UTILITY_TYPE_PATH.to_owned(),
        payload: catalogue.to_ron().into_bytes(),
        dependencies: Vec::new(),
    })
}

/// A custom data asset (`.data.ron`, ADR-7232) keeps its text: its value type is the game's, so the
/// cook checks the document (version, type path, RON value) and the game's load decodes the value.
fn cook_data_asset(source: &Path, bytes: &[u8]) -> Result<CookedTable, CookError> {
    let meta = AssetDatabase::load_or_create_meta(source, "data_asset").map_err(CookError::Db)?;
    let text = text_of(bytes)?;
    regia_asset::data_asset::DataAssetFile::header(text).map_err(|e| table_error(e.to_string()))?;
    Ok(CookedTable {
        importer: "data_asset".to_owned(),
        settings_hash: *blake3::hash(bytes).as_bytes(),
        id: meta.id,
        type_path: regia_asset::data_asset::DATA_ASSET_TYPE_PATH.to_owned(),
        payload: text.replace("\r\n", "\n").into_bytes(),
        dependencies: Vec::new(),
    })
}

/// Validates and encodes the data table `source`.
pub(crate) fn cook_table(source: &Path, bytes: &[u8], project_root: &Path) -> Result<CookedTable, CookError> {
    let name = source.file_name().and_then(|n| n.to_str()).unwrap_or("");
    match importer_of(name) {
        Some("curve_table") => cook_curve_table(source, name, bytes),
        Some("chooser") => cook_chooser(source, bytes, project_root),
        Some("camera") => cook_camera(source, bytes),
        Some("uianim") => cook_uianim(source, bytes),
        Some("data_table") => cook_data_table(source, name, bytes),
        Some("abilities") => cook_ability_asset(source, bytes, project_root),
        Some("netprofile") => cook_net_profile(source, bytes),
        Some("data_asset") => cook_data_asset(source, bytes),
        Some("det_utility") => cook_det_utility(source, bytes),
        Some(_) => cook_smart_object(source, bytes),
        None => Err(CookError::UnsupportedExtension(name.to_owned())),
    }
}

/// The registry tags of a cooked table: `kind`, and `result` for a chooser, read from its payload
/// in `cooked_file`. Empty for any other type path.
pub(crate) fn registry_tags(type_path: &str, cooked_file: &Path) -> BTreeMap<String, String> {
    let mut tags = BTreeMap::new();
    if type_path == CURVE_TABLE_TYPE_PATH {
        tags.insert("kind".to_owned(), "curve_table".to_owned());
    } else if type_path == SMART_OBJECT_DEF_TYPE_PATH {
        tags.insert("kind".to_owned(), "smart_object".to_owned());
    } else if type_path == CAMERA_ASSET_TYPE_PATH {
        tags.insert("kind".to_owned(), "camera".to_owned());
    } else if type_path == UIANIM_TYPE_PATH {
        tags.insert("kind".to_owned(), "ui_anim".to_owned());
    } else if type_path == ABILITIES_COOKED_TYPE_PATH {
        tags.insert("kind".to_owned(), "ability_asset".to_owned());
    } else if type_path == regia_net::profile_assets::NET_PROFILE_TYPE_PATH {
        tags.insert("kind".to_owned(), "net_profile".to_owned());
    } else if type_path == regia_ai::det::DET_UTILITY_TYPE_PATH {
        tags.insert("kind".to_owned(), "det_utility".to_owned());
    } else if type_path == regia_asset::data_asset::DATA_ASSET_TYPE_PATH {
        tags.insert("kind".to_owned(), "data_asset".to_owned());
        let header = std::fs::read(cooked_file).ok().and_then(|file| {
            let text = regia_asset::data_asset::cooked_text(&file, type_path).ok()?;
            regia_asset::data_asset::DataAssetFile::header(text).ok()
        });
        if let Some(header) = header {
            tags.insert("type".to_owned(), header.type_path);
        }
    } else if type_path == DATA_TABLE_COOKED_TYPE_PATH {
        tags.insert("kind".to_owned(), "data_table".to_owned());
        let table = std::fs::read(cooked_file).ok().and_then(|file| {
            let (_, payload) = regia_asset::decode_cooked_file(&file).ok()?;
            CookedDataTable::from_bytes(payload).ok()
        });
        if let Some(table) = table.filter(|t| !t.row_type().is_empty()) {
            tags.insert("row_type".to_owned(), table.row_type().to_owned());
        }
    } else if type_path == CHOOSER_TYPE_PATH {
        tags.insert("kind".to_owned(), "chooser".to_owned());
        let table = std::fs::read(cooked_file).ok().and_then(|file| {
            let (_, payload) = regia_asset::decode_cooked_file(&file).ok()?;
            ChooserTable::from_cooked(payload).ok()
        });
        if let Some(table) = table {
            tags.insert("result".to_owned(), table.result_tag());
        }
    }
    tags
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_tables_are_named_ignoring_case() {
        for name in [
            "a.curves.ron",
            "a.curves.csv",
            "A.Chooser.RON",
            "a.SMARTOBJECT.ron",
            "a.table.csv",
            "weapons.Table.RON",
            "menus.UIAnim.ron",
        ] {
            assert!(is_data_table_name(name), "{name}");
        }
        for name in [
            "a.ron",
            "a.table",
            "curves.ron",
            "chooser.ron",
            "a.curves",
            "table.ron",
        ] {
            assert!(!is_data_table_name(name), "{name}");
        }
    }

    /// The ability formats cook under their exact (lowercase) suffixes, as the runtime loads them
    /// (ADR-9370).
    #[test]
    fn ability_formats_are_data_tables_by_their_exact_suffix() {
        for name in ["hero.attrs.ron", "poison.effect.ron", "fire_bolt.ability.ron"] {
            assert!(is_data_table_name(name), "{name}");
            assert_eq!(importer_of(name), Some("abilities"), "{name}");
        }
        for name in ["Hero.Attrs.RON", "effect.ron", "a.ability", "ability.ron.bak"] {
            assert_ne!(importer_of(name), Some("abilities"), "{name}");
        }
    }
}
