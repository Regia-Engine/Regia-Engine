// SPDX-License-Identifier: MPL-2.0
//! Texture-set detection and material generation (spec 22 T2.14, ADR-3320): a folder of PBR
//! maps named `<base>_<suffix>.<ext>` becomes one `.material.ron` per base name, with no
//! hand-written RON.
//!
//! Detection groups raster sources by folder and base name using the one shared suffix table
//! ([`TEXTURE_SUFFIX_TABLE`]; the cook's usage inference in the pipeline derives from the same
//! table, extended there only), extended per project by `Regia.toml`:
//!
//! ```toml
//! [import.texture_sets]
//! normal_green = "opengl"        # "opengl" (default) or "directx": the project's green
//!                                # convention for set normals whose suffix doesn't say
//! [import.texture_sets.suffixes]
//! base_color = ["diff"]          # extra suffix words per slot, merged with the defaults
//! ```
//!
//! Generation writes, per set: the `.meta` of every member (its cook `usage`, plus
//! `normal_green` for a DirectX-convention normal and `orm_channel_order` for a packed map that
//! does not store `ORM` — both applied at cook time, never on disk), and
//! `<base>.material.ron` in the set's folder — never over an existing material. The material
//! names the members' asset ids, so the cook's chunking pulls the maps into the same pak. Slots
//! the standard material cannot hold are reported, never guessed: a separate
//! `_Roughness`/`_Metallic` map has no slot until the packed-ORM packer exists (lane
//! texture-cook-coverage-pack; `texture_pack.rs` is not on this base), and a separate `_Opacity`
//! map only switches the alpha mode — the standard shader reads alpha from the base colour
//! map's alpha channel.
//!
//! This module lives in `regia_asset` (not the pipeline) because the agent protocol — a
//! runtime crate, which may not depend on the desktop-only pipeline (`docs/06` § 7,
//! ADR-3320) — exposes it as `assets.texture_sets`.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::{
    AlphaMode, AssetId, MaterialAssetRon, MetaSidecar, ParallaxParams, TextureCookUsage, TextureSlot,
    meta_path_for,
};

/// Raster sources the set detector considers (the extensions the standalone raster cook cooks).
const RASTER_EXTENSIONS: [&str; 5] = ["png", "jpg", "jpeg", "tga", "bmp"];

/// The normal-map green (Y) convention of a source (spec 22 T2.14, ADR-3320): OpenGL-style
/// normals store +Y up (green up), DirectX-style store −Y (green down). The renderer consumes
/// the OpenGL convention; a DirectX source is flipped at cook, never on disk.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NormalGreenSetting {
    /// OpenGL convention (+Y): the default; no transform.
    #[default]
    Opengl,
    /// DirectX convention (−Y): the cook flips the green channel.
    Directx,
}

/// The channel order a packed occlusion-roughness-metallic source stores (spec 22 T2.14,
/// ADR-3320). The renderer consumes `ORM` (occlusion R, roughness G, metallic B, the glTF
/// metallic-roughness layout with occlusion in R); a source in another order is swizzled at
/// cook, never on disk.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrmChannelOrderSetting {
    /// Occlusion R, roughness G, metallic B: the native layout (`_ORM`, `_ARM`).
    #[default]
    Orm,
    /// AO R, roughness G, metallic B: the same layout under its `_ARM` name.
    Arm,
    /// Roughness R, metallic G, AO B (`_RMA`).
    Rma,
    /// Metallic R, roughness G, AO B (`_MRA`).
    Mra,
}

impl OrmChannelOrderSetting {
    /// The order a packed-map suffix names.
    #[must_use]
    pub fn from_suffix(suffix: &str) -> Self {
        match suffix {
            "arm" => Self::Arm,
            "rma" => Self::Rma,
            "mra" => Self::Mra,
            _ => Self::Orm,
        }
    }
}

/// Rewrites `rgba` in place for cook: flips the green channel of a DirectX-convention normal
/// map and swizzles a packed map in another channel order to `ORM`. `usage` gates both (the
/// flip only touches normal maps, the swizzle only linear packed data).
pub fn apply_channel_transforms(
    rgba: &mut [u8],
    usage: TextureCookUsage,
    green: NormalGreenSetting,
    order: OrmChannelOrderSetting,
) {
    if usage == TextureCookUsage::NormalMap && green == NormalGreenSetting::Directx {
        for px in rgba.as_chunks_mut::<4>().0 {
            px[1] = 255 - px[1];
        }
    }
    if usage == TextureCookUsage::ColorLinear && order != OrmChannelOrderSetting::Orm {
        for px in rgba.as_chunks_mut::<4>().0 {
            let [r, g, b, _] = [px[0], px[1], px[2], px[3]];
            match order {
                OrmChannelOrderSetting::Rma => {
                    // Source R roughness, G metallic, B AO -> occlusion, roughness, metallic.
                    px[0] = b;
                    px[1] = r;
                    px[2] = g;
                }
                OrmChannelOrderSetting::Mra => {
                    // Source R metallic, G roughness, B AO -> occlusion, roughness, metallic.
                    px[0] = b;
                    px[1] = g;
                    px[2] = r;
                }
                OrmChannelOrderSetting::Orm | OrmChannelOrderSetting::Arm => {}
            }
        }
    }
}

/// The texture-set slot a source file fills (spec 22 T2.14, ADR-3320): one PBR map of a set
/// named `<base>_<suffix>.<ext>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextureSetSlot {
    /// sRGB base colour (`_BaseColor`, `_Albedo`, `_Diffuse`, `_Color`).
    BaseColor,
    /// Tangent-space normal map (`_Normal`, `_N`, `_Nrm`, `_NormalGL`, `_NormalDX`).
    Normal,
    /// Linear roughness (`_Roughness`, `_Rough`, `_R`).
    Roughness,
    /// Linear metallic (`_Metallic`, `_Metal`, `_M`).
    Metallic,
    /// Linear ambient occlusion (`_AO`, `_Occlusion`).
    AmbientOcclusion,
    /// Packed occlusion-roughness-metallic (`_ORM`, `_ARM`, `_RMA`, `_MRA`): the channel order
    /// per suffix is [`OrmChannelOrderSetting::from_suffix`].
    Orm,
    /// Linear height (`_Height`, `_Displacement`, `_Bump`).
    Height,
    /// sRGB emissive (`_Emissive`, `_Emission`).
    Emissive,
    /// Linear opacity (`_Opacity`, `_Alpha`, `_Mask`).
    Opacity,
}

impl TextureSetSlot {
    /// Every slot, in table order.
    pub const ALL: [Self; 9] = [
        Self::BaseColor,
        Self::Normal,
        Self::Roughness,
        Self::Metallic,
        Self::AmbientOcclusion,
        Self::Orm,
        Self::Height,
        Self::Emissive,
        Self::Opacity,
    ];

    /// The slot's stable snake-case label (the `[import.texture_sets.suffixes]` TOML key).
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::BaseColor => "base_color",
            Self::Normal => "normal",
            Self::Roughness => "roughness",
            Self::Metallic => "metallic",
            Self::AmbientOcclusion => "ao",
            Self::Orm => "orm",
            Self::Height => "height",
            Self::Emissive => "emissive",
            Self::Opacity => "opacity",
        }
    }

    /// Parses [`Self::label`].
    #[must_use]
    pub fn from_label(label: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|slot| slot.label() == label)
    }
}

/// One row of the shared texture-suffix table: the lowercase words (no separators) that name a
/// slot at the end of a file stem, the cook usage a standalone source with one of them infers,
/// and the texture-set slot it fills (`None`: the word is cook-only and never joins a set —
/// ambiguous packings such as `_RM` are reported as leftovers by the set detector rather than
/// guessed).
#[derive(Clone, Copy, Debug)]
pub struct TextureSuffixRow {
    /// The texture-set slot, when a file with this suffix belongs to a set.
    pub slot: Option<TextureSetSlot>,
    /// How a standalone source with one of these suffixes cooks.
    pub usage: TextureCookUsage,
    /// The lowercase suffix words (the last `_`/`-`/`.`/space separated word of the stem).
    pub suffixes: &'static [&'static str],
}

/// The one shared texture-suffix table (spec 22 T2.14, ADR-3320). The cook's usage inference
/// (the pipeline's `usage_from_file_name`) and this detector both derive from it; extend it
/// here only, never fork it. The words a row lists are disjoint from every other row's, so
/// lookup order does not matter.
pub const TEXTURE_SUFFIX_TABLE: &[TextureSuffixRow] = &[
    TextureSuffixRow {
        slot: Some(TextureSetSlot::BaseColor),
        usage: TextureCookUsage::ColorSrgb,
        suffixes: &["basecolor", "basecolour", "albedo", "diffuse", "color", "colour"],
    },
    TextureSuffixRow {
        slot: Some(TextureSetSlot::Normal),
        usage: TextureCookUsage::NormalMap,
        suffixes: &[
            "n",
            "nrm",
            "nor",
            "norm",
            "normal",
            "normals",
            "normalmap",
            "normalgl",
            "normaldx",
            "nml",
        ],
    },
    TextureSuffixRow {
        slot: Some(TextureSetSlot::Roughness),
        usage: TextureCookUsage::ColorLinear,
        suffixes: &["roughness", "rough", "r"],
    },
    TextureSuffixRow {
        slot: Some(TextureSetSlot::Metallic),
        usage: TextureCookUsage::ColorLinear,
        suffixes: &["metal", "metallic", "metalness", "m"],
    },
    TextureSuffixRow {
        slot: Some(TextureSetSlot::AmbientOcclusion),
        usage: TextureCookUsage::ColorLinear,
        suffixes: &["ao", "occlusion", "ambientocclusion"],
    },
    TextureSuffixRow {
        slot: Some(TextureSetSlot::Orm),
        usage: TextureCookUsage::ColorLinear,
        suffixes: &["orm", "arm", "rma", "mra", "occlusionroughnessmetallic"],
    },
    TextureSuffixRow {
        slot: Some(TextureSetSlot::Height),
        usage: TextureCookUsage::ColorLinear,
        suffixes: &["height", "disp", "displacement", "bump"],
    },
    TextureSuffixRow {
        slot: Some(TextureSetSlot::Emissive),
        usage: TextureCookUsage::ColorSrgb,
        suffixes: &["emissive", "emission"],
    },
    TextureSuffixRow {
        slot: Some(TextureSetSlot::Opacity),
        usage: TextureCookUsage::ColorLinear,
        suffixes: &["opacity", "alpha", "mask", "masks"],
    },
    // Cook-only words: standalone maps that never join a texture set (an `_RM`/`_MR` packing is
    // ambiguous about which channel owns which value; the detector reports it as a leftover).
    TextureSuffixRow {
        slot: None,
        usage: TextureCookUsage::ColorLinear,
        suffixes: &[
            "rm",
            "mr",
            "metallicroughness",
            "spec",
            "specular",
            "gloss",
            "glossiness",
            "data",
            "curvature",
            "cavity",
        ],
    },
];

/// The last separator-delimited word of `path`'s stem, lowercased (the suffix word the shared
/// table matches). A one-word name is its own suffix; an empty stem has none.
#[must_use]
pub fn suffix_word(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?.to_ascii_lowercase();
    stem.split(['_', '-', '.', ' '])
        .rfind(|w| !w.is_empty())
        .map(str::to_string)
}

/// The usage a standalone image cooks as by its file name: the last `_`/`-`/`.`/space separated
/// word of the stem (or the whole stem) matched against the shared [`TEXTURE_SUFFIX_TABLE`];
/// sRGB colour when no row claims it.
#[must_use]
pub fn usage_from_file_name(path: &Path) -> TextureCookUsage {
    let Some(last) = suffix_word(path) else {
        return TextureCookUsage::ColorSrgb;
    };
    TEXTURE_SUFFIX_TABLE
        .iter()
        .find(|row| row.suffixes.contains(&last.as_str()))
        .map_or(TextureCookUsage::ColorSrgb, |row| row.usage)
}

/// The texture-set slot `path`'s suffix word fills, if a table row claims it for sets.
#[must_use]
pub fn texture_set_slot_from_file_name(path: &Path) -> Option<TextureSetSlot> {
    let last = suffix_word(path)?;
    TEXTURE_SUFFIX_TABLE
        .iter()
        .find(|row| row.suffixes.contains(&last.as_str()))
        .and_then(|row| row.slot)
}

/// The packed-map channel order `path`'s suffix word names ([`OrmChannelOrderSetting::Orm`]
/// for non-packed names).
#[must_use]
pub fn orm_channel_order_from_file_name(path: &Path) -> OrmChannelOrderSetting {
    suffix_word(path).map_or(OrmChannelOrderSetting::Orm, |w| {
        OrmChannelOrderSetting::from_suffix(&w)
    })
}

/// Why a raster file did not join a set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LeftoverReason {
    /// The file's suffix word names no table row (a colour map is usually intentional).
    UnknownSuffix,
    /// The suffix word is cook-only (`_RM`, `_Specular`, …): its slot is ambiguous, so the
    /// detector reports it instead of guessing.
    CookOnlySuffix,
    /// The file's whole stem is the suffix word (`normal.png`): there is no base name to group by.
    NoBaseName,
}

impl LeftoverReason {
    /// The stable snake-case label (JSON and text reports).
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::UnknownSuffix => "unknown_suffix",
            Self::CookOnlySuffix => "cook_only_suffix",
            Self::NoBaseName => "no_base_name",
        }
    }
}

/// A raster file the scan left out of every set.
#[derive(Clone, Debug, Serialize)]
pub struct TextureSetLeftover {
    /// The source file.
    pub path: PathBuf,
    /// Why it did not join a set.
    pub reason: LeftoverReason,
}

/// One source file that claimed a set slot.
#[derive(Clone, Debug, Serialize)]
pub struct TextureSetMember {
    /// The source file.
    pub path: PathBuf,
    /// The matched suffix word (lowercase).
    pub suffix: String,
}

/// More than one file claimed one slot; none is used (the detector never guesses).
#[derive(Clone, Debug, Serialize)]
pub struct TextureSetConflict {
    /// The contested slot.
    pub slot: TextureSetSlot,
    /// Every file that claimed the slot, sorted for a stable report.
    pub candidates: Vec<PathBuf>,
}

/// One detected texture set: the sources of one base name in one folder.
#[derive(Clone, Debug, Serialize)]
pub struct TextureSet {
    /// The base name the members share (also the material's file name).
    pub name: String,
    /// The folder the members live in; the material is written here.
    pub folder: PathBuf,
    /// At most one member per slot; a contested slot is in `conflicts`, not here.
    pub slots: BTreeMap<TextureSetSlot, TextureSetMember>,
    /// Slots more than one file claimed.
    pub conflicts: Vec<TextureSetConflict>,
}

/// The result of scanning a folder for texture sets.
#[derive(Clone, Debug, Default, Serialize)]
pub struct TextureSetScan {
    /// Every detected set, sorted by folder then name.
    pub sets: Vec<TextureSet>,
    /// Raster files that joined no set.
    pub leftovers: Vec<TextureSetLeftover>,
}

/// The `[import.texture_sets]` project settings: the default green convention and extra
/// per-slot suffix words merged with the shared table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextureSetSettings {
    /// The green convention for set normal maps whose suffix does not say (`_NormalGL` /
    /// `_NormalDX` override it). The default is OpenGL (+Y).
    pub normal_green: NormalGreenSetting,
    /// Extra suffix words per slot, merged with [`TEXTURE_SUFFIX_TABLE`]'s defaults (an extra
    /// word wins over a default word of another slot).
    pub extra_suffixes: BTreeMap<TextureSetSlot, Vec<String>>,
}

impl Default for TextureSetSettings {
    fn default() -> Self {
        Self {
            normal_green: NormalGreenSetting::Opengl,
            extra_suffixes: BTreeMap::new(),
        }
    }
}

/// The `[import.texture_sets]` TOML keys.
const SETTINGS_KEYS: [&str; 2] = ["normal_green", "suffixes"];

impl TextureSetSettings {
    /// The settings from the `Regia.toml` text `manifest`: the `[import.texture_sets]` table,
    /// or the defaults when it is absent.
    ///
    /// # Errors
    /// The TOML does not parse, or `[import.texture_sets]` holds an unknown key, a bad
    /// `normal_green` value, or a `suffixes` entry that is not a list of lowercase words naming
    /// a known slot (the message names it).
    pub fn from_manifest_toml(manifest: &str) -> Result<Self, String> {
        let root: toml::Table = manifest.parse().map_err(|e| format!("Regia.toml: {e}"))?;
        let Some(import) = root.get("import").and_then(toml::Value::as_table) else {
            return Ok(Self::default());
        };
        let Some(table) = import.get("texture_sets") else {
            return Ok(Self::default());
        };
        let table = table
            .as_table()
            .ok_or("Regia.toml: [import.texture_sets] must be a table")?;
        for key in table.keys() {
            if !SETTINGS_KEYS.contains(&key.as_str()) {
                return Err(format!(
                    "Regia.toml: [import.texture_sets] has unknown key `{key}` (known: {})",
                    SETTINGS_KEYS.join(", ")
                ));
            }
        }
        let mut settings = Self::default();
        match table.get("normal_green") {
            None => {}
            Some(toml::Value::String(s)) => {
                settings.normal_green = match s.as_str() {
                    "opengl" => NormalGreenSetting::Opengl,
                    "directx" => NormalGreenSetting::Directx,
                    _ => {
                        return Err(format!(
                            "Regia.toml: [import.texture_sets] normal_green `{s}` is not `opengl` or `directx`"
                        ));
                    }
                };
            }
            Some(_) => {
                return Err("Regia.toml: [import.texture_sets] normal_green must be a string".to_string());
            }
        }
        if let Some(suffixes) = table.get("suffixes") {
            let suffixes = suffixes
                .as_table()
                .ok_or("Regia.toml: [import.texture_sets.suffixes] must be a table")?;
            for (slot, words) in suffixes {
                let Some(slot) = TextureSetSlot::from_label(slot) else {
                    return Err(format!(
                        "Regia.toml: [import.texture_sets.suffixes] unknown slot `{slot}` (known: {})",
                        TextureSetSlot::ALL
                            .iter()
                            .map(|s| s.label())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                };
                let words = words.as_array().ok_or_else(|| {
                    format!(
                        "Regia.toml: [import.texture_sets.suffixes] {} must be an array of strings",
                        slot.label()
                    )
                })?;
                for word in words {
                    let word = word.as_str().ok_or_else(|| {
                        format!(
                            "Regia.toml: [import.texture_sets.suffixes] {} must be an array of strings",
                            slot.label()
                        )
                    })?;
                    if word.is_empty() || !word.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
                    {
                        return Err(format!(
                            "Regia.toml: [import.texture_sets.suffixes] {} word `{word}` must be lowercase ascii",
                            slot.label()
                        ));
                    }
                    settings
                        .extra_suffixes
                        .entry(slot)
                        .or_default()
                        .push(word.to_string());
                }
            }
        }
        Ok(settings)
    }

    /// The settings from `<project_root>/Regia.toml` (the default when the file is missing).
    ///
    /// # Errors
    /// As [`Self::from_manifest_toml`]; an unreadable (not missing) manifest.
    pub fn for_project(project_root: &Path) -> Result<Self, String> {
        match std::fs::read_to_string(project_root.join("Regia.toml")) {
            Ok(text) => Self::from_manifest_toml(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("Regia.toml: {e}")),
        }
    }

    /// The slot `word` fills: an extra suffix first, then the shared table. `Some(None)` marks
    /// a cook-only word (it names a row with no set slot); `None` marks an unknown word.
    fn match_word(&self, word: &str) -> Option<Option<TextureSetSlot>> {
        for (slot, words) in &self.extra_suffixes {
            if words.iter().any(|w| w == word) {
                return Some(Some(*slot));
            }
        }
        TEXTURE_SUFFIX_TABLE
            .iter()
            .find(|row| row.suffixes.contains(&word))
            .map(|row| row.slot)
    }
}

/// A generation/scan error with the path it concerns.
#[derive(Debug)]
pub struct TextureSetError {
    /// What failed, human-readable.
    pub message: String,
    /// The file or folder the failure concerns.
    pub path: PathBuf,
}

impl std::fmt::Display for TextureSetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.message)
    }
}

impl std::error::Error for TextureSetError {}

fn io_error(path: &Path, source: &std::io::Error) -> TextureSetError {
    TextureSetError {
        message: source.to_string(),
        path: path.to_path_buf(),
    }
}

/// Splits `stem` (already lowercased) into `(base, suffix word)`: the suffix word is the last
/// `_`/`-`/`.`/space separated token; the base is what precedes it. `None` when the stem is
/// only the suffix word or empty.
fn split_base_suffix(stem: &str) -> Option<(String, String)> {
    let is_sep = |b: u8| matches!(b, b'_' | b'-' | b'.' | b' ');
    let bytes = stem.as_bytes();
    let mut end = bytes.len();
    while end > 0 && is_sep(bytes[end - 1]) {
        end -= 1;
    }
    let mut start = end;
    while start > 0 && !is_sep(bytes[start - 1]) {
        start -= 1;
    }
    if start == 0 {
        return None;
    }
    let mut base_end = start - 1;
    while base_end > 0 && is_sep(bytes[base_end - 1]) {
        base_end -= 1;
    }
    if base_end == 0 {
        return None;
    }
    Some((stem[..base_end].to_string(), stem[start..end].to_string()))
}

/// Scans `folder` for texture sets (see the module docs). `recursive` descends into
/// sub-folders; each folder's sets are detected on their own (a set never spans folders).
///
/// # Errors
/// `folder` cannot be read.
pub fn scan(
    folder: &Path,
    recursive: bool,
    settings: &TextureSetSettings,
) -> Result<TextureSetScan, TextureSetError> {
    let mut files = Vec::new();
    collect_rasters(folder, recursive, &mut files)?;
    files.sort();

    let mut groups: BTreeMap<(PathBuf, String), TextureSet> = BTreeMap::new();
    let mut leftovers = Vec::new();
    for path in files {
        let Some(word) = suffix_word(&path) else {
            leftovers.push(TextureSetLeftover {
                path,
                reason: LeftoverReason::NoBaseName,
            });
            continue;
        };
        let Some(slot) = settings.match_word(&word) else {
            leftovers.push(TextureSetLeftover {
                path,
                reason: LeftoverReason::UnknownSuffix,
            });
            continue;
        };
        let Some(slot) = slot else {
            leftovers.push(TextureSetLeftover {
                path,
                reason: LeftoverReason::CookOnlySuffix,
            });
            continue;
        };
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();
        let Some((base, matched)) = split_base_suffix(&stem) else {
            leftovers.push(TextureSetLeftover {
                path,
                reason: LeftoverReason::NoBaseName,
            });
            continue;
        };
        debug_assert_eq!(matched, word);
        let folder = path.parent().unwrap_or(folder).to_path_buf();
        let set = groups
            .entry((folder.clone(), base.clone()))
            .or_insert_with(|| TextureSet {
                name: base,
                folder,
                slots: BTreeMap::new(),
                conflicts: Vec::new(),
            });
        if let Some(existing) = set.slots.get(&slot) {
            let mut candidates = vec![existing.path.clone(), path];
            candidates.sort();
            set.conflicts.push(TextureSetConflict { slot, candidates });
            set.slots.remove(&slot);
        } else if let Some(conflict) = set.conflicts.iter_mut().find(|c| c.slot == slot) {
            conflict.candidates.push(path);
            conflict.candidates.sort();
        } else {
            set.slots.insert(slot, TextureSetMember { path, suffix: word });
        }
    }
    let sets = groups.into_values().collect();
    Ok(TextureSetScan { sets, leftovers })
}

fn collect_rasters(folder: &Path, recursive: bool, out: &mut Vec<PathBuf>) -> Result<(), TextureSetError> {
    let entries = std::fs::read_dir(folder).map_err(|e| io_error(folder, &e))?;
    for entry in entries {
        let entry = entry.map_err(|e| io_error(folder, &e))?;
        let path = entry.path();
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_dir() {
            if recursive {
                collect_rasters(&path, recursive, out)?;
            }
            continue;
        }
        if !meta.is_file() {
            continue;
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if RASTER_EXTENSIONS.contains(&ext.as_str()) {
            out.push(path);
        }
    }
    Ok(())
}

/// The meta `settings` fields a member needs: `usage` always; `normal_green` for a
/// DirectX-convention normal (set, or removed when the convention is OpenGL); and
/// `orm_channel_order` for a packed map that is not native `ORM` (set, or removed for a
/// native order). `None` removes the field.
fn member_fields(
    slot: TextureSetSlot,
    suffix: &str,
    settings: &TextureSetSettings,
) -> Vec<(&'static str, Option<&'static str>)> {
    let usage = match slot {
        TextureSetSlot::BaseColor | TextureSetSlot::Emissive => Some("color"),
        TextureSetSlot::Normal => Some("normal"),
        TextureSetSlot::Roughness
        | TextureSetSlot::Metallic
        | TextureSetSlot::AmbientOcclusion
        | TextureSetSlot::Orm
        | TextureSetSlot::Height
        | TextureSetSlot::Opacity => Some("linear"),
    };
    let mut fields = vec![("usage", usage)];
    if slot == TextureSetSlot::Normal {
        let green = match suffix {
            "normaldx" => Some("directx"),
            "normalgl" => None,
            _ if settings.normal_green == NormalGreenSetting::Directx => Some("directx"),
            _ => None,
        };
        fields.push(("normal_green", green));
    }
    if slot == TextureSetSlot::Orm {
        let order = match suffix {
            "rma" => Some("rma"),
            "mra" => Some("mra"),
            _ => None,
        };
        fields.push(("orm_channel_order", order));
    }
    fields
}

/// Splices `fields` into a `.meta` `settings` string (see [`member_fields`]). With no existing
/// settings the result is a fresh `(usage: Some(...), ...)`; otherwise the existing fields are
/// kept, `Some` values replace or append, and `None` values remove. Returns `None` when the
/// existing string is not the flat `(key: value, ...)` shape this schema writes (the caller
/// keeps the artist's string and reports a warning).
fn splice_settings(existing: Option<&str>, fields: &[(&str, Option<&str>)]) -> Option<String> {
    let sets: Vec<(&str, &str)> = fields
        .iter()
        .filter_map(|(key, value)| value.map(|v| (*key, v)))
        .collect();
    let fresh = |sets: &[(&str, &str)]| {
        let body = sets
            .iter()
            .map(|(key, value)| format!("{key}: Some({value})"))
            .collect::<Vec<_>>()
            .join(", ");
        format!("({body})")
    };
    let text = match existing.map(str::trim) {
        None | Some("") => return Some(fresh(&sets)),
        Some(text) => text,
    };
    let inner = text.strip_prefix('(')?.strip_suffix(')')?;
    // Fields are split at depth-0 commas; values in this schema are flat, so depth should
    // stay zero outside `Some(...)` — anything else is not a shape we rewrite.
    let mut depth = 0i32;
    let mut pieces = vec![String::new()];
    for ch in inner.chars() {
        match ch {
            '(' => {
                depth += 1;
                pieces.last_mut()?.push(ch);
            }
            ')' => {
                depth -= 1;
                if depth < 0 {
                    return None;
                }
                pieces.last_mut()?.push(ch);
            }
            ',' if depth == 0 => pieces.push(String::new()),
            _ => pieces.last_mut()?.push(ch),
        }
    }
    if depth != 0 {
        return None;
    }
    let mut items: Vec<(String, String)> = Vec::new();
    for piece in pieces {
        let piece = piece.trim();
        if piece.is_empty() {
            continue;
        }
        let (name, value) = piece.split_once(':')?;
        let (name, value) = (name.trim(), value.trim());
        let name_ok = !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
        // `None`, or `Some(...)` without nested parens (a label, number or bool).
        let value_ok = value == "None"
            || (value.starts_with("Some(")
                && value.ends_with(')')
                && !value["Some(".len()..value.len() - 1].contains(['(', ')']));
        if !name_ok || !value_ok {
            return None;
        }
        if !items.iter().any(|(n, _)| n == name) {
            items.push((name.to_string(), value.to_string()));
        }
    }
    let removes: Vec<&str> = fields
        .iter()
        .filter(|(_, value)| value.is_none())
        .map(|(key, _)| *key)
        .collect();
    items.retain(|(name, _)| !removes.contains(&name.as_str()));
    for (key, value) in sets {
        match items.iter_mut().find(|(name, _)| name == key) {
            Some(entry) => entry.1 = format!("Some({value})"),
            None => items.push((key.to_string(), format!("Some({value})"))),
        }
    }
    let body = items
        .iter()
        .map(|(name, value)| format!("{name}: {value}"))
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!("({body})"))
}

/// One set's generation outcome (also the dry-run report: with `write: false` nothing is
/// written and the texture ids hold only the ids already on disk).
#[derive(Clone, Debug, Serialize)]
pub struct TextureSetOutcome {
    /// The set's base name.
    pub name: String,
    /// The folder the material is written to.
    pub folder: PathBuf,
    /// The material's path.
    pub material_path: PathBuf,
    /// A material already exists there: it is left untouched, nothing is written for this set.
    pub material_exists: bool,
    /// The material was written (only with `write: true` and no existing material).
    pub written: bool,
    /// The member per slot that the material references (asset id when known — `None` marks a
    /// member whose `.meta` does not exist yet and is created on write).
    pub textures: BTreeMap<TextureSetSlot, TextureSlotReport>,
    /// The material's alpha mode (cutout when the set ships an opacity map).
    pub alpha_mode: AlphaMode,
    /// Members the standard material cannot hold, with the reason (never guessed, never
    /// silently dropped).
    pub unreferenced: Vec<UnreferencedMap>,
    /// Slots more than one file claimed.
    pub conflicts: Vec<TextureSetConflict>,
    /// A member's existing `.meta` or settings could not be parsed and was left as is (the
    /// cook falls back to its defaults for it).
    pub warnings: Vec<String>,
}

/// One member the material references.
#[derive(Clone, Debug, Serialize)]
pub struct TextureSlotReport {
    /// The source file.
    pub path: PathBuf,
    /// The asset id (`None` in a dry run when the `.meta` does not exist yet).
    pub id: Option<AssetId>,
    /// The meta `settings` string the generator writes for the member.
    pub settings: String,
}

/// A member the material cannot reference.
#[derive(Clone, Debug, Serialize)]
pub struct UnreferencedMap {
    /// The source file.
    pub path: PathBuf,
    /// Why the standard material has no slot for it.
    pub reason: String,
}

/// Builds the material document for one set. `ids` must resolve every slot the material
/// references (generation with `write: true`).
fn build_material(set: &TextureSet, ids: &BTreeMap<TextureSetSlot, AssetId>) -> MaterialAssetRon {
    let mut material = MaterialAssetRon::default_standard();
    material.name = Some(set.name.clone());
    let mut base = material.base.clone();
    for &slot in set.slots.keys() {
        let Some(&id) = ids.get(&slot) else {
            continue;
        };
        let tex = Some(TextureSlot::Asset(id));
        match slot {
            TextureSetSlot::BaseColor => base.base_color_map = tex,
            TextureSetSlot::Normal => base.normal_map = tex,
            TextureSetSlot::Emissive => {
                base.emissive_map = tex;
                // A bare emissive factor of 0 multiplies the map to black: a set ships a map
                // to emit, so the factor is white (ADR-3320).
                base.emissive = [1.0, 1.0, 1.0];
            }
            TextureSetSlot::Orm => {
                base.orm_map = tex;
                base.orm_packed = true;
            }
            TextureSetSlot::AmbientOcclusion => {
                // Occlusion-only until a packed map exists; roughness/metallic factors stay.
                if !set.slots.contains_key(&TextureSetSlot::Orm)
                    && !set.slots.contains_key(&TextureSetSlot::Roughness)
                    && !set.slots.contains_key(&TextureSetSlot::Metallic)
                {
                    base.orm_map = tex;
                    base.orm_packed = false;
                }
            }
            // No slot in the standard material (reported in `unreferenced_maps`).
            TextureSetSlot::Roughness | TextureSetSlot::Metallic | TextureSetSlot::Opacity => {}
            TextureSetSlot::Height => {
                let mut hs = material.hard_surface.clone().unwrap_or_default();
                hs.parallax = Some(ParallaxParams {
                    height_map: tex,
                    ..ParallaxParams::default()
                });
                material.hard_surface = Some(hs);
            }
        }
    }
    material.base = base;
    if set.slots.contains_key(&TextureSetSlot::Opacity) {
        material.render_state.alpha_mode = AlphaMode::Mask;
    }
    material
}

/// The members the standard material cannot hold (reported, never guessed).
fn unreferenced_maps(set: &TextureSet) -> Vec<UnreferencedMap> {
    let mut out = Vec::new();
    if set.slots.contains_key(&TextureSetSlot::Roughness) || set.slots.contains_key(&TextureSetSlot::Metallic)
    {
        // Without a packed map, roughness/metallic have no slot: the packed-ORM packer is
        // another lane's (texture-cook-coverage-pack, `texture_pack.rs` is not on this base).
        for slot in [TextureSetSlot::Roughness, TextureSetSlot::Metallic] {
            if let Some(member) = set.slots.get(&slot) {
                out.push(UnreferencedMap {
                    path: member.path.clone(),
                    reason: "the standard material reads roughness/metallic from a packed ORM map; separate maps are packed when the packed-ORM cook lands (lane texture-cook-coverage-pack)".to_string(),
                });
            }
        }
        if !set.slots.contains_key(&TextureSetSlot::Orm)
            && let Some(ao) = set.slots.get(&TextureSetSlot::AmbientOcclusion)
        {
            out.push(UnreferencedMap {
                path: ao.path.clone(),
                reason: "occlusion-only is recorded only when no roughness/metallic maps ship; the AO of this set belongs in the packed map the packer will build".to_string(),
            });
        }
    }
    if set.slots.contains_key(&TextureSetSlot::Orm)
        && let Some(ao) = set.slots.get(&TextureSetSlot::AmbientOcclusion)
    {
        out.push(UnreferencedMap {
            path: ao.path.clone(),
            reason: "the material reads occlusion from the packed ORM map's red channel; a separate AO map next to it is not referenced".to_string(),
        });
    }
    if let Some(opacity) = set.slots.get(&TextureSetSlot::Opacity) {
        out.push(UnreferencedMap {
            path: opacity.path.clone(),
            reason: "the standard material reads alpha from the base colour map's alpha channel; the opacity map sets the material's alpha mode".to_string(),
        });
    }
    out
}

/// Generates materials for every scanned set. With `write: false` this is a dry run: existing
/// `.meta` ids are read for the report but nothing is written. With `write: true` the members'
/// `.meta` files are created/updated (usage, green convention, channel order) and one
/// `<base>.material.ron` per set is written next to its sources — never over an existing one. A set
/// whose material exists is left untouched, its members' `.meta` files included; a member `.meta`
/// that does not parse is a warning and is left as it is — and in a write run that set's material
/// is not written either, so `material_exists` never freezes a material that is missing a slot
/// (fix the `.meta` and rerun).
///
/// # Errors
/// A `.meta` or material cannot be written.
pub fn generate(
    scan: &TextureSetScan,
    settings: &TextureSetSettings,
    write: bool,
) -> Result<Vec<TextureSetOutcome>, TextureSetError> {
    let mut outcomes = Vec::new();
    for set in &scan.sets {
        outcomes.push(generate_set(set, settings, write)?);
    }
    Ok(outcomes)
}

fn generate_set(
    set: &TextureSet,
    settings: &TextureSetSettings,
    write: bool,
) -> Result<TextureSetOutcome, TextureSetError> {
    let material_path = set.folder.join(format!("{}.material.ron", set.name));
    let material_exists = material_path.is_file();
    let mut warnings = Vec::new();
    let mut textures: BTreeMap<TextureSetSlot, TextureSlotReport> = BTreeMap::new();
    let mut ids: BTreeMap<TextureSetSlot, AssetId> = BTreeMap::new();
    // An existing material leaves the whole set untouched (`material_exists`): its members'
    // `.meta` files are neither created nor rewritten (an artist may have changed them since).
    let write_members = write && !material_exists;

    // The first member `.meta` that does not parse: a write run must not write the set's
    // material without that member's slot, because `material_exists` would freeze the
    // partial material (fixrev2-asset-k, finding 1 of the corr-asset review).
    let mut unreadable_meta: Option<PathBuf> = None;
    for (&slot, member) in &set.slots {
        let fields = member_fields(slot, &member.suffix, settings);
        let meta_path = meta_path_for(&member.path);
        // A `.meta` that does not parse is reported and left as it is (never replaced, and the
        // run goes on with the other members and sets).
        let mut unreadable = false;
        let existing = std::fs::read(&meta_path).ok().and_then(|bytes| {
            MetaSidecar::from_ron(&bytes)
                .map_err(|e| {
                    unreadable = true;
                    unreadable_meta.get_or_insert_with(|| meta_path.clone());
                    warnings.push(format!(
                        "{}: existing .meta ignored ({e}); the cook's defaults apply",
                        member.path.display()
                    ));
                })
                .ok()
        });
        let write_member = write_members && !unreadable;
        let id = match (&existing, write_member) {
            (Some(meta), _) => Some(meta.id),
            (None, true) => Some(load_or_create_meta(&member.path)?.id),
            (None, false) => None,
        };
        let mut report_settings = String::new();
        if material_exists {
            if let Some(meta) = &existing {
                report_settings = meta.settings.clone().unwrap_or_default();
            }
        } else if let Some(meta) = &existing {
            report_settings = match splice_settings(meta.settings.as_deref(), &fields) {
                Some(text) => {
                    if write_member {
                        write_meta(&member.path, meta, text.clone())?;
                    }
                    text
                }
                None => {
                    warnings.push(format!(
                        "{}: unrecognised .meta settings left as they are",
                        member.path.display()
                    ));
                    meta.settings.clone().unwrap_or_default()
                }
            };
        } else if write_member {
            // The freshly created meta carries no settings yet: splice against none.
            let text = splice_settings(None, &fields).expect("fresh settings splice");
            let meta = std::fs::read(&meta_path)
                .ok()
                .and_then(|bytes| MetaSidecar::from_ron(&bytes).ok())
                .ok_or_else(|| TextureSetError {
                    message: "created .meta does not read back".to_string(),
                    path: member.path.clone(),
                })?;
            write_meta(&member.path, &meta, text.clone())?;
            report_settings = text;
        }
        if let Some(id) = id {
            ids.insert(slot, id);
        }
        textures.insert(
            slot,
            TextureSlotReport {
                path: member.path.clone(),
                id,
                settings: report_settings,
            },
        );
    }

    let unreferenced = unreferenced_maps(set);
    let mut written = false;
    if write && !material_exists && !ids.is_empty() {
        if let Some(meta_path) = &unreadable_meta {
            // Writing the material now would freeze it without the unreadable member's
            // slot: the user fixing the `.meta` and rerunning finds `material_exists` and
            // the slot stays missing. Skip the write; the rerun writes the complete set.
            warnings.push(format!(
                "material not written: fix {} and rerun",
                meta_path.display()
            ));
        } else {
            let material = build_material(set, &ids);
            let text = ron::ser::to_string_pretty(&material, regia_core::ron_pretty()).map_err(|e| {
                TextureSetError {
                    message: format!("cannot serialize material: {e}"),
                    path: material_path.clone(),
                }
            })?;
            // Never over an existing material, even one a racing writer created after the
            // check above: the create is atomic and a file already there wins.
            written = write_new_atomic(&material_path, text.as_bytes())?;
        }
    }
    let alpha_mode = if set.slots.contains_key(&TextureSetSlot::Opacity) {
        AlphaMode::Mask
    } else {
        AlphaMode::Opaque
    };
    Ok(TextureSetOutcome {
        name: set.name.clone(),
        folder: set.folder.clone(),
        material_path,
        material_exists,
        written,
        textures,
        alpha_mode,
        unreferenced,
        conflicts: set.conflicts.clone(),
        warnings,
    })
}

/// Reads the source's `.meta`, or creates it (a fresh random id) without replacing one a
/// racing writer published meanwhile.
fn load_or_create_meta(source: &Path) -> Result<MetaSidecar, TextureSetError> {
    let path = meta_path_for(source);
    if let Ok(bytes) = std::fs::read(&path) {
        return MetaSidecar::from_ron(&bytes).map_err(|e| TextureSetError {
            message: format!("cannot parse .meta: {e}"),
            path,
        });
    }
    let ext = source
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("png")
        .to_string();
    let meta = MetaSidecar::new(&ext, AssetId::new_random());
    let text = meta.to_ron_string().map_err(|e| TextureSetError {
        message: format!("cannot serialize .meta: {e}"),
        path: path.clone(),
    })?;
    if write_new_atomic(&path, text.as_bytes())? {
        return Ok(meta);
    }
    let bytes = std::fs::read(&path).map_err(|e| io_error(&path, &e))?;
    MetaSidecar::from_ron(&bytes).map_err(|e| TextureSetError {
        message: format!("cannot parse .meta after a racing create: {e}"),
        path,
    })
}

/// Writes `bytes` to `path` only if `path` does not exist yet (a racing creator's file wins
/// and is read back by the caller). `true` when this call created the file.
fn write_new_atomic(path: &Path, bytes: &[u8]) -> Result<bool, TextureSetError> {
    use std::io::Write as _;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_TMP: AtomicU64 = AtomicU64::new(0);
    let n = NEXT_TMP.fetch_add(1, Ordering::Relaxed);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}-{n}.tmp", std::process::id()));
    let tmp = path.with_file_name(name);
    if let Err(e) = std::fs::write(&tmp, bytes) {
        let _ = std::fs::remove_file(&tmp);
        return Err(io_error(path, &e));
    }
    // Windows: a no-replace move, not a hard link, so the `.meta` stays replaceable at once
    // (the hard link's unlinked temp stays delete-pending while a scanner reads it, ADR-8504).
    #[cfg(windows)]
    let linked = regia_platform::rename_no_replace(&tmp, path);
    #[cfg(not(windows))]
    let linked = std::fs::hard_link(&tmp, path);
    if cfg!(not(windows)) || linked.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    match linked {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(_) => match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(mut file) => {
                if let Err(e) = file.write_all(bytes) {
                    drop(file);
                    let _ = std::fs::remove_file(path);
                    return Err(io_error(path, &e));
                }
                Ok(true)
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
            Err(e) => Err(io_error(path, &e)),
        },
    }
}

/// Writes `meta` (with its `settings` replaced) to the source's `.meta`, atomically.
fn write_meta(source: &Path, meta: &MetaSidecar, settings: String) -> Result<(), TextureSetError> {
    let mut meta = meta.clone();
    meta.settings = Some(settings);
    let text = meta.to_ron_string().map_err(|e| TextureSetError {
        message: format!("cannot serialize .meta: {e}"),
        path: source.to_path_buf(),
    })?;
    write_atomic(&meta_path_for(source), text.as_bytes())
}

/// Writes `bytes` to `path` through a unique temporary file in the same folder plus a rename,
/// so a reader never sees a half-written file.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), TextureSetError> {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}-{}.tmp", std::process::id(), unique_tmp_id()));
    let tmp = path.with_file_name(name);
    let written = std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, path));
    if let Err(e) = written {
        // A failed write or rename must not leave the temporary file behind.
        let _ = std::fs::remove_file(&tmp);
        return Err(io_error(path, &e));
    }
    Ok(())
}

fn unique_tmp_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_TMP: AtomicU64 = AtomicU64::new(0);
    NEXT_TMP.fetch_add(1, Ordering::Relaxed)
}

/// Renders the outcomes as plain text (the CLI default).
#[must_use]
pub fn format_report(outcomes: &[TextureSetOutcome], leftovers: &[TextureSetLeftover]) -> String {
    let mut out = String::new();
    for o in outcomes {
        let _ = writeln!(
            out,
            "{}{}:",
            o.name,
            if o.material_exists {
                " (material exists — untouched)"
            } else if o.written {
                " (written)"
            } else {
                " (dry run)"
            }
        );
        for (slot, tex) in &o.textures {
            let id = tex
                .id
                .map(|id| id.to_string())
                .unwrap_or_else(|| "(new on write)".to_string());
            let _ = writeln!(
                out,
                "  {}: {} [{}] {}",
                slot.label(),
                tex.path.display(),
                id,
                tex.settings
            );
        }
        let _ = writeln!(out, "  alpha_mode: {:?}", o.alpha_mode);
        for u in &o.unreferenced {
            let _ = writeln!(out, "  unreferenced: {} ({})", u.path.display(), u.reason);
        }
        for c in &o.conflicts {
            let names: Vec<String> = c.candidates.iter().map(|p| p.display().to_string()).collect();
            let _ = writeln!(
                out,
                "  conflict: {} claimed by {}",
                c.slot.label(),
                names.join(" and ")
            );
        }
        for w in &o.warnings {
            let _ = writeln!(out, "  warning: {w}");
        }
    }
    for l in leftovers {
        let _ = writeln!(out, "leftover: {} ({})", l.path.display(), l.reason.label());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_suffix_split_keeps_separators_and_case_words() {
        assert_eq!(
            split_base_suffix("hull_basecolor"),
            Some(("hull".to_string(), "basecolor".to_string()))
        );
        assert_eq!(
            split_base_suffix("t_rock-nrm"),
            Some(("t_rock".to_string(), "nrm".to_string()))
        );
        assert_eq!(
            split_base_suffix("wing normal"),
            Some(("wing".to_string(), "normal".to_string()))
        );
        // Trailing separators are trimmed; a one-word stem has no base.
        assert_eq!(split_base_suffix("normal"), None);
        assert_eq!(split_base_suffix("___"), None);
        assert_eq!(split_base_suffix(""), None);
        // The base keeps interior separators; only the trailing run goes.
        assert_eq!(
            split_base_suffix("a_b_c_ao"),
            Some(("a_b_c".to_string(), "ao".to_string()))
        );
    }

    #[test]
    fn scan_is_recursive_but_a_set_never_spans_folders() {
        let dir = regia_test_utils::unique_temp_dir("texset-scan");
        let nested = dir.join("nested");
        std::fs::create_dir_all(&nested).expect("dirs");
        for (folder, name) in [
            (dir.as_path(), "rock_BaseColor.png"),
            (dir.as_path(), "rock_Normal.png"),
            (nested.as_path(), "rock_Roughness.png"),
        ] {
            std::fs::write(folder.join(name), b"not-a-real-png").expect("file");
        }
        // Non-recursive: the nested roughness is invisible, one two-member set.
        let flat = scan(&dir, false, &TextureSetSettings::default()).expect("scan");
        assert_eq!(flat.sets.len(), 1);
        assert_eq!(flat.sets[0].slots.len(), 2);
        // Recursive: the nested file is a set of its own (sets never span folders).
        let deep = scan(&dir, true, &TextureSetSettings::default()).expect("scan");
        assert_eq!(deep.sets.len(), 2);
        assert!(deep.sets.iter().any(|s| s.slots.len() == 1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Spec 22 § T2.14a: the ambiguous packings `_RM`, `_MR` and `_MetallicRoughness` are
    /// cook-only. They join no set (not even beside a base colour of the same base name) and are
    /// reported as `cook_only_suffix` leftovers (ADR-5628).
    #[test]
    fn ambiguous_metal_rough_packings_are_cook_only_leftovers() {
        let packed = ["hull_RM.png", "hull_MR.png", "hull_MetallicRoughness.png"];
        for name in packed {
            assert_eq!(texture_set_slot_from_file_name(Path::new(name)), None, "{name}");
        }
        let dir = regia_test_utils::unique_temp_dir("texset-ambiguous");
        std::fs::create_dir_all(&dir).expect("dir");
        for name in packed.iter().chain(&["hull_BaseColor.png"]) {
            std::fs::write(dir.join(name), b"not-a-real-png").expect("file");
        }
        let found = scan(&dir, false, &TextureSetSettings::default()).expect("scan");
        assert_eq!(found.sets.len(), 1, "the base colour alone makes the set");
        assert_eq!(found.sets[0].slots.len(), 1, "no packed map joins it");
        let mut cook_only: Vec<String> = found
            .leftovers
            .iter()
            .filter(|l| l.reason == LeftoverReason::CookOnlySuffix)
            .filter_map(|l| l.path.file_name().map(|n| n.to_string_lossy().into_owned()))
            .collect();
        cook_only.sort();
        assert_eq!(
            cook_only,
            ["hull_MR.png", "hull_MetallicRoughness.png", "hull_RM.png"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn settings_splice_fresh_replace_append_and_remove() {
        // Fresh: only the given fields.
        assert_eq!(
            splice_settings(
                None,
                &[("usage", Some("normal")), ("normal_green", Some("directx"))]
            ),
            Some("(usage: Some(normal), normal_green: Some(directx))".to_string())
        );
        // Replace one field, keep the others in order, append the new one.
        assert_eq!(
            splice_settings(
                Some("(usage: Some(color), max_size: Some(512))"),
                &[("usage", Some("normal"))]
            ),
            Some("(usage: Some(normal), max_size: Some(512))".to_string())
        );
        // Remove a field; `None` leaves nothing behind.
        assert_eq!(
            splice_settings(
                Some("(usage: Some(normal), normal_green: Some(directx))"),
                &[("usage", Some("normal")), ("normal_green", None)]
            ),
            Some("(usage: Some(normal))".to_string())
        );
        // Not the flat shape we rewrite: keep the artist's string (None reports it).
        assert_eq!(
            splice_settings(Some("(usage: Some((nested)))"), &[("usage", Some("normal"))]),
            None
        );
        assert_eq!(
            splice_settings(Some("not ron"), &[("usage", Some("normal"))]),
            None
        );
        assert_eq!(
            splice_settings(Some("(unbalanced: Some(1)"), &[("usage", Some("normal"))]),
            None
        );
    }

    /// corr-asset: an AO map beside a packed ORM map is not referenced by the material, so the
    /// report lists it as unreferenced instead of dropping it silently.
    #[test]
    fn corr_asset_ao_beside_orm_is_reported_unreferenced() {
        let dir = regia_test_utils::unique_temp_dir("texset-ao-orm");
        for name in ["rock_basecolor.png", "rock_orm.png", "rock_ao.png"] {
            std::fs::write(dir.join(name), b"x").expect("file");
        }
        let settings = TextureSetSettings::default();
        let scan = scan(&dir, false, &settings).expect("scan");
        assert_eq!(scan.sets.len(), 1);
        let out = generate(&scan, &settings, false).expect("dry run");
        let ao = dir.join("rock_ao.png");
        assert!(
            out[0].unreferenced.iter().any(|u| u.path == ao),
            "{:?}",
            out[0].unreferenced
        );
    }

    /// corr-asset: a member `.meta` that does not parse is a warning; the write run carries on
    /// and leaves that file as it is. fixrev2-asset-k: the run must not write the set's
    /// material without the unreadable member's slot either — `material_exists` would freeze
    /// the partial material. Fixing the `.meta` and rerunning writes the complete material.
    #[test]
    fn corr_asset_unreadable_meta_is_a_warning_not_an_error() {
        let dir = regia_test_utils::unique_temp_dir("texset-badmeta");
        for name in ["rock_basecolor.png", "rock_normal.png"] {
            std::fs::write(dir.join(name), b"x").expect("file");
        }
        let normal_meta = crate::meta_path_for(&dir.join("rock_normal.png"));
        std::fs::write(&normal_meta, b"garbage").expect("bad meta");
        let settings = TextureSetSettings::default();
        let scan = scan(&dir, false, &settings).expect("scan");
        let out = generate(&scan, &settings, true).expect("a bad .meta is not an error");
        assert!(!out[0].warnings.is_empty());
        assert!(
            out[0].warnings.iter().any(|w| w.contains("material not written")),
            "{:?}",
            out[0].warnings
        );
        assert_eq!(std::fs::read(&normal_meta).expect("read"), b"garbage");
        assert!(crate::meta_path_for(&dir.join("rock_basecolor.png")).is_file());
        let material = dir.join("rock.material.ron");
        assert!(
            !material.is_file(),
            "an unreadable member .meta freezes no partial material"
        );
        // Fixing the `.meta` and rerunning writes the complete material, both slots listed.
        let fixed = MetaSidecar::new("png", AssetId::new_random());
        std::fs::write(&normal_meta, fixed.to_ron_string().expect("ron")).expect("fixed meta");
        let out = generate(&scan, &settings, true).expect("rerun");
        assert!(out[0].written, "{:?}", out[0].warnings);
        assert!(material.is_file());
        assert!(
            out[0].textures.values().all(|t| t.id.is_some()),
            "every member is listed: {:?}",
            out[0].textures
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// corr-asset: a set whose material already exists is left untouched, its members' `.meta`
    /// files included.
    #[test]
    fn corr_asset_existing_material_leaves_member_metas_alone() {
        let dir = regia_test_utils::unique_temp_dir("texset-exists");
        for name in ["rock_basecolor.png", "rock_normal.png"] {
            std::fs::write(dir.join(name), b"x").expect("file");
        }
        let settings = TextureSetSettings::default();
        let scan = scan(&dir, false, &settings).expect("scan");
        let first = generate(&scan, &settings, true).expect("write");
        assert!(first[0].written);
        // The artist changes the normal map's usage after the material exists.
        let normal = dir.join("rock_normal.png");
        let meta = load_or_create_meta(&normal).expect("meta");
        write_meta(&normal, &meta, "(usage: Some(color))".to_string()).expect("artist edit");
        let before = std::fs::read(crate::meta_path_for(&normal)).expect("read meta");
        let again = generate(&scan, &settings, true).expect("again");
        assert!(again[0].material_exists && !again[0].written);
        assert_eq!(
            std::fs::read(crate::meta_path_for(&normal)).expect("read meta"),
            before,
            "the member .meta is untouched"
        );
    }

    #[test]
    fn audfix_third_texture_candidate_is_kept() {
        let dir = regia_test_utils::unique_temp_dir("texset-third");
        for name in ["rock_albedo.png", "rock_basecolor.png", "rock_diffuse.png"] {
            std::fs::write(dir.join(name), b"x").expect("file");
        }
        let scan = scan(&dir, false, &TextureSetSettings::default()).expect("scan");
        assert_eq!(scan.sets.len(), 1);
        let set = &scan.sets[0];
        assert!(set.slots.is_empty(), "a contested slot is not filled");
        assert!(scan.leftovers.is_empty(), "{:?}", scan.leftovers);
        assert_eq!(set.conflicts.len(), 1);
        assert_eq!(set.conflicts[0].slot, TextureSetSlot::BaseColor);
        let names: Vec<_> = set.conflicts[0]
            .candidates
            .iter()
            .map(|p| p.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string())
            .collect();
        assert_eq!(
            names,
            vec![
                "rock_albedo.png".to_string(),
                "rock_basecolor.png".to_string(),
                "rock_diffuse.png".to_string(),
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Corr-low (corr-asset #13): a failed rename left `<name>.<pid>-<n>.tmp` behind.
    #[test]
    fn corr_low_a_failed_rename_leaves_no_temporary_file() {
        let dir = regia_test_utils::unique_temp_dir("texture_sets_tmp_leak");
        // A non-empty directory in the target's place: the rename onto it fails.
        let target = dir.join("rock.material.ron");
        std::fs::create_dir_all(target.join("inside")).expect("blocking directory");
        assert!(write_atomic(&target, b"x").is_err());
        let names: Vec<String> = std::fs::read_dir(&dir)
            .expect("dir")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            ["rock.material.ron"],
            "a temporary file was left: {names:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
