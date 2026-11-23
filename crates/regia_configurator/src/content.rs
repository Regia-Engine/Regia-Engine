// SPDX-License-Identifier: MPL-2.0
//! The other documents the configurator runtime reads (ADR-2534): the piece recipe
//! (`jewelry.piece_recipe/1`, plan `interfaces.md` § 3), the part kit (`jewelry.part_kit/1`, § 5),
//! the material library (`jewelry.material_library/1`, § 6) and the optional shop photo
//! (`jewelry.shop_photo/1`, § 4, ADR-2558).
//!
//! A recipe's `bake` block (`transport_tables`, `probes`, `front_still`) is not read: the viewer
//! bakes nothing and draws the same with any flags, so a file may set them either way or leave the
//! block out. It is still part of the recipe's hash, like every other field.
//!
//! Only the fields the runtime uses are typed; every other field is accepted and kept out of the
//! way (the contract checks in `checks` validate the full documents). Nothing here renders: the
//! runtime turns picks into a [`crate::runtime::Look`] (which part, which material, which cut) and
//! the viewer draws it.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;

use crate::graph::LoadError;
use crate::resolve::Picks;

/// The piece recipe's schema id.
pub const RECIPE_SCHEMA: &str = "jewelry.piece_recipe/1";
/// The part kit's schema id.
pub const PART_KIT_SCHEMA: &str = "jewelry.part_kit/1";
/// The material library's schema id.
pub const MATERIAL_LIBRARY_SCHEMA: &str = "jewelry.material_library/1";

/// The shop photo's schema id.
pub const SHOP_PHOTO_SCHEMA: &str = "jewelry.shop_photo/1";

/// One design's recipe: its parts, gem slots, material zones and what each option group drives.
#[derive(Debug, Clone, Deserialize)]
pub struct Recipe {
    /// `jewelry.piece_recipe/1`.
    pub schema: String,
    /// The owning house.
    #[serde(default)]
    pub house: String,
    /// The design the recipe builds.
    pub design_key: String,
    /// The products that share the design.
    #[serde(default)]
    pub product_keys: Vec<String>,
    /// Bumped on every change.
    #[serde(default = "one")]
    pub recipe_version: u32,
    /// Named parameters (`mq_l`), in mm.
    #[serde(default)]
    pub values: BTreeMap<String, f64>,
    /// Part instances.
    #[serde(default)]
    pub parts: Vec<RecipePart>,
    /// Gem slots.
    #[serde(default)]
    pub gem_slots: Vec<GemSlot>,
    /// Material zones (`body`, `gem:s1`).
    #[serde(default)]
    pub material_zones: Vec<MaterialZone>,
    /// Option group -> what it changes.
    #[serde(default)]
    pub drives: Vec<Drive>,
    /// Groups that change nothing visible.
    #[serde(default)]
    pub no_visual: Vec<String>,
    /// The view limits; binding for every camera.
    #[serde(default)]
    pub view_range: ViewRange,
    /// The default camera.
    #[serde(default)]
    pub camera_default: Option<CameraDefault>,
    /// The wear presets.
    #[serde(default)]
    pub wear: Option<Value>,
    /// `sha256:<hex>` of the RFC 8785 text of the whole document (set on load).
    #[serde(skip)]
    pub hash: String,
}

fn one() -> u32 {
    1
}

/// One part instance of a recipe.
#[derive(Debug, Clone, Deserialize)]
pub struct RecipePart {
    /// Unique within the recipe.
    pub instance_id: String,
    /// `<house>/<part_id>@<kit_version>`.
    pub part_ref: String,
    /// Part parameters.
    #[serde(default)]
    pub params: BTreeMap<String, Value>,
}

/// One gem slot of a recipe.
#[derive(Debug, Clone, Deserialize)]
pub struct GemSlot {
    /// `s1`, `s2`, ...
    pub slot_id: String,
    /// `center`, `accent`, ...
    #[serde(default)]
    pub role: String,
    /// The cut type (`marquise`, `round_brilliant`).
    #[serde(default)]
    pub cut: String,
    /// Sizes in mm: numbers or `$name` references to `values`.
    #[serde(default)]
    pub size_mm: Vec<Value>,
    /// Stones in the slot.
    #[serde(default = "one")]
    pub count: u32,
    /// The material zone the slot's stone takes (`gem:s1`).
    #[serde(default)]
    pub material_slot: Option<String>,
}

/// One material zone.
#[derive(Debug, Clone, Deserialize)]
pub struct MaterialZone {
    /// `body`, `gem:s1`, ...
    pub zone: String,
    /// `metal`, `gem`, ...
    #[serde(default)]
    pub kind: String,
}

/// What an option group drives.
#[derive(Debug, Clone, Deserialize)]
pub struct Drive {
    /// The option group name.
    pub group: String,
    /// The target.
    pub to: DriveTarget,
}

/// A drive's target: a material zone, a named value or a part swap.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DriveTarget {
    /// The zone whose material the group's value picks.
    #[serde(default)]
    pub material_zone: Option<String>,
    /// The named value the group's label maps to (through `map`).
    #[serde(default)]
    pub value: Option<String>,
    /// The part instance the group's label swaps (through `map`).
    #[serde(default)]
    pub part_swap: Option<String>,
    /// Label -> number (for `value`) or part ref (for `part_swap`).
    #[serde(default)]
    pub map: BTreeMap<String, Value>,
}

/// The view limits of a piece (`interfaces.md` § 3 `view_range`; defaults from the operator's
/// decision D4: front view, ±30° tilt, no spin).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ViewRange {
    /// The piece's front direction.
    #[serde(default = "front_axis")]
    pub front_axis: [f64; 3],
    /// Most tilt away from the front axis, in degrees.
    #[serde(default = "tilt_max")]
    pub tilt_max_deg: f64,
    /// Pan limits in mm: `[[x_min, x_max], [y_min, y_max]]`.
    #[serde(default)]
    pub pan_bounds_mm: Option<[[f64; 2]; 2]>,
    /// `none`, `rock`, `about_front_axis` or `free`.
    #[serde(default = "spin_none")]
    pub spin: String,
    /// Most rock either way, in degrees (spin `rock` only).
    #[serde(default)]
    pub rock_max_deg: Option<f64>,
}

fn front_axis() -> [f64; 3] {
    [0.0, 0.0, 1.0]
}
fn tilt_max() -> f64 {
    30.0
}
fn spin_none() -> String {
    "none".to_owned()
}

impl Default for ViewRange {
    fn default() -> Self {
        Self {
            front_axis: front_axis(),
            tilt_max_deg: tilt_max(),
            pan_bounds_mm: None,
            spin: spin_none(),
            rock_max_deg: None,
        }
    }
}

/// The recipe's default camera.
#[derive(Debug, Clone, Deserialize)]
pub struct CameraDefault {
    /// Distance from the piece, in mm.
    #[serde(default)]
    pub distance_mm: Option<f64>,
    /// Lens focal length, in mm.
    #[serde(default)]
    pub focal_length_mm: Option<f64>,
    /// Aperture.
    #[serde(default)]
    pub f_stop: Option<f64>,
}

impl Recipe {
    /// Parse a recipe from JSON bytes and compute its hash.
    pub fn from_json(bytes: &[u8]) -> Result<Self, LoadError> {
        let value: Value =
            serde_json::from_slice(bytes).map_err(|e| LoadError::bad(format!("piece recipe: {e}")))?;
        Self::from_value(value)
    }

    /// Parse a recipe from a JSON value and compute its hash.
    pub fn from_value(value: Value) -> Result<Self, LoadError> {
        let hash = crate::checks::sha256_ref(&value)
            .ok_or_else(|| LoadError::bad("piece recipe: a number cannot be written canonically"))?;
        let mut recipe: Self =
            serde_json::from_value(value).map_err(|e| LoadError::bad(format!("piece recipe: {e}")))?;
        if recipe.schema != RECIPE_SCHEMA {
            return Err(LoadError::bad(format!(
                "`{}` is not a piece recipe (schema `{}`)",
                recipe.design_key, recipe.schema
            )));
        }
        if !(0.0..=90.0).contains(&recipe.view_range.tilt_max_deg) {
            return Err(LoadError::bad(format!(
                "recipe `{}`: view_range.tilt_max_deg {} is outside 0..=90",
                recipe.design_key, recipe.view_range.tilt_max_deg
            )));
        }
        recipe.hash = hash;
        Ok(recipe)
    }

    /// The drives of `group`, in recipe order.
    pub fn drives_of<'a>(&'a self, group: &'a str) -> impl Iterator<Item = &'a Drive> + 'a {
        self.drives.iter().filter(move |d| d.group == group)
    }

    /// A slot size: numbers as given, `$name` looked up in `values` (with `overrides` first).
    pub fn slot_size(&self, slot: &GemSlot, overrides: &BTreeMap<String, f64>) -> Option<Vec<f64>> {
        slot.size_mm
            .iter()
            .map(|v| match v {
                Value::Number(n) => n.as_f64(),
                Value::String(s) => {
                    let name = s.strip_prefix('$')?;
                    overrides.get(name).or_else(|| self.values.get(name)).copied()
                }
                _ => None,
            })
            .collect()
    }
}

/// One ordinary photo of one stocked piece (`photo.json`): an optional by-eye reference. Nothing
/// depends on it, and it holds no image, only file names beside it.
#[derive(Debug, Clone, Deserialize)]
pub struct ShopPhoto {
    /// `jewelry.shop_photo/1`.
    pub schema: String,
    /// The folder name under `captures/<house>/<product_slug>/`.
    #[serde(default)]
    pub photo_id: Option<String>,
    /// The owning house.
    pub house: String,
    /// The piece.
    pub product_key: String,
    /// The variant in the photo.
    pub variant_key: String,
    /// The picks in the photo; they resolve to `variant_key`.
    pub picks: Picks,
    /// When it was taken (RFC 3339).
    pub taken_at: String,
    /// A role (`shop`), never a person's name.
    pub operator: String,
    /// The image files in the folder.
    pub files: Vec<ShopPhotoFile>,
    /// A ruler is in frame.
    #[serde(default)]
    pub ruler_in_frame: Option<bool>,
    /// Measured sizes in mm, by name.
    #[serde(default)]
    pub measurements_mm: BTreeMap<String, f64>,
}

/// One image of a [`ShopPhoto`].
#[derive(Debug, Clone, Deserialize)]
pub struct ShopPhotoFile {
    /// The file name in the folder.
    pub file: String,
    /// `top`, `side` or `three_quarter`.
    pub view: String,
}

impl ShopPhoto {
    /// Parse a `photo.json`, and check its schema id and that each file stays in the folder.
    pub fn from_json(bytes: &[u8]) -> Result<Self, LoadError> {
        let photo: Self =
            serde_json::from_slice(bytes).map_err(|e| LoadError::bad(format!("shop photo: {e}")))?;
        if photo.schema != SHOP_PHOTO_SCHEMA {
            return Err(LoadError::bad(format!(
                "`{}` is not a shop photo (schema `{}`)",
                photo.product_key, photo.schema
            )));
        }
        if let Some(f) = photo
            .files
            .iter()
            .find(|f| !crate::source::clean_path(&f.file) || f.file.contains('/'))
        {
            return Err(LoadError::bad(format!(
                "shop photo of `{}`: `{}` is not a file name in the folder",
                photo.product_key, f.file
            )));
        }
        Ok(photo)
    }
}

/// A part kit: generic or house parts and the gem cuts actually offered.
#[derive(Debug, Clone, Deserialize)]
pub struct PartKit {
    /// `jewelry.part_kit/1`.
    pub schema: String,
    /// The kit id.
    pub kit_id: String,
    /// The kit version (a number or a string).
    #[serde(default)]
    pub kit_version: Value,
    /// The owning house, or `shared`.
    pub house: String,
    /// The parts.
    #[serde(default)]
    pub parts: Vec<KitPart>,
    /// The gem cuts.
    #[serde(default)]
    pub gem_cuts: Vec<GemCut>,
}

/// One kit part (the fields the runtime reads).
#[derive(Debug, Clone, Deserialize)]
pub struct KitPart {
    /// The part id.
    pub part_id: String,
    /// `post`, `end_base`, `setting`, `bezel`, ...
    #[serde(default)]
    pub kind: String,
    /// The glTF file.
    #[serde(default)]
    pub glb: Option<String>,
    /// The node in it.
    #[serde(default)]
    pub node: Option<String>,
}

/// One gem cut of a kit (the fields the runtime reads).
#[derive(Debug, Clone, Deserialize)]
pub struct GemCut {
    /// The cut id.
    pub cut_id: String,
    /// The cut type (`round_brilliant`, `marquise`).
    pub cut_type: String,
    /// Sizes in mm.
    #[serde(default)]
    pub size_mm: Vec<f64>,
}

impl PartKit {
    /// Parse a kit from JSON bytes.
    pub fn from_json(bytes: &[u8]) -> Result<Self, LoadError> {
        let kit: Self =
            serde_json::from_slice(bytes).map_err(|e| LoadError::bad(format!("part kit: {e}")))?;
        if kit.schema != PART_KIT_SCHEMA {
            return Err(LoadError::bad(format!(
                "`{}` is not a part kit (schema `{}`)",
                kit.kit_id, kit.schema
            )));
        }
        Ok(kit)
    }
}

/// Tolerance when matching a slot size to a kit cut, in mm.
const SIZE_TOLERANCE_MM: f64 = 0.005;

/// Find a part by ref (`<house>/<part_id>@<version>`; the version is informative).
pub fn find_part<'a>(kits: &'a [PartKit], part_ref: &str) -> Option<(&'a PartKit, &'a KitPart)> {
    let (house, rest) = part_ref.split_once('/')?;
    let part_id = rest.split_once('@').map_or(rest, |(p, _)| p);
    kits.iter()
        .filter(|k| k.house == house)
        .find_map(|k| k.parts.iter().find(|p| p.part_id == part_id).map(|p| (k, p)))
}

/// Find the kit cut of `cut_type` whose size matches `size_mm`.
pub fn find_cut<'a>(kits: &'a [PartKit], cut_type: &str, size_mm: &[f64]) -> Option<&'a GemCut> {
    kits.iter().flat_map(|k| k.gem_cuts.iter()).find(|c| {
        c.cut_type == cut_type
            && c.size_mm.len() == size_mm.len()
            && c.size_mm
                .iter()
                .zip(size_mm)
                .all(|(a, b)| (a - b).abs() <= SIZE_TOLERANCE_MM)
    })
}

/// A material library: every material the renderer accepts, keyed by the catalogue labels it maps from.
#[derive(Debug, Clone, Deserialize)]
pub struct MaterialLibrary {
    /// `jewelry.material_library/1`.
    pub schema: String,
    /// The library version.
    #[serde(default)]
    pub library_version: Value,
    /// The entries.
    #[serde(default)]
    pub materials: Vec<MaterialEntry>,
}

/// One material library entry (the fields the runtime reads; the look data stays in `raw`).
#[derive(Debug, Clone, Deserialize)]
pub struct MaterialEntry {
    /// The material id.
    pub material_id: String,
    /// `conductor`, `anodized`, `gem_dielectric`, `gem_special`, `finish`.
    #[serde(default)]
    pub kind: String,
    /// The name a person sees.
    #[serde(default)]
    pub display_name: String,
    /// `<group>=<label>` option values the entry maps from.
    #[serde(default)]
    pub catalogue_facets: Vec<String>,
}

impl MaterialLibrary {
    /// Parse a library from JSON bytes.
    pub fn from_json(bytes: &[u8]) -> Result<Self, LoadError> {
        let lib: Self =
            serde_json::from_slice(bytes).map_err(|e| LoadError::bad(format!("material library: {e}")))?;
        if lib.schema != MATERIAL_LIBRARY_SCHEMA {
            return Err(LoadError::bad(format!(
                "not a material library (schema `{}`)",
                lib.schema
            )));
        }
        Ok(lib)
    }

    /// The material an option value maps to: an exact `<group>=<label>` facet first, then a facet
    /// with the same label under any group, then (for a gem value) an entry whose id is the
    /// gem's `material`. `None` when nothing maps (the viewer shows its fallback).
    pub fn lookup(&self, group: &str, label: &str, gem: Option<&Value>) -> Option<&MaterialEntry> {
        let exact = format!("{group}={label}");
        self.materials
            .iter()
            .find(|m| m.catalogue_facets.contains(&exact))
            .or_else(|| {
                self.materials.iter().find(|m| {
                    m.catalogue_facets
                        .iter()
                        .any(|f| f.split_once('=').is_some_and(|(_, l)| l == label))
                })
            })
            .or_else(|| {
                let material = gem?.get("material")?.as_str()?;
                self.materials.iter().find(|m| m.material_id == material)
            })
    }
}
