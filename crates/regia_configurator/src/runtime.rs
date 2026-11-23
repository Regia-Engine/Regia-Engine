// SPDX-License-Identifier: MPL-2.0
//! The configurator runtime (ADR-2534): one open product, the current picks, and what they mean.
//!
//! - [`Configurator::open`] loads a product's documents, checks the tenant may see its house, and
//!   starts from the first sold combination (so a viewer has something to show at once).
//! - [`Configurator::options`] lists every option group and value, each value marked `selectable`
//!   only when some sold combination contains it together with the other picks. The configurator
//!   never offers a pick that would not resolve.
//! - [`Configurator::set_pick`] applies one tap: [`SetMode::Strict`] refuses a value no sold
//!   combination has with the current picks; [`SetMode::Adjust`] keeps the new value and drops the
//!   picks that conflict with it (reported), then fills any open axis or required add-on from the
//!   first sold combination. A pick no sold combination contains at all is refused in both modes.
//! - [`Configurator::look`] turns the picks into what to draw: each material zone's material, each
//!   part (after swaps), each gem slot's cut and stone, and the recipe's named values.
//! - [`Configurator::build`] writes the `jewelry.build/1` JSON (no cost, no client data), checked
//!   against the build contract before it is returned; [`Configurator::load_build`] reopens a saved
//!   build and re-resolves it against the current snapshot.
//!
//! The invariant the tests fuzz: after any sequence of accepted picks, [`Configurator::resolve`]
//! is orderable, and its variant is one the option graph sells.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::Interface;
use crate::content::{MaterialEntry, Recipe, ViewRange, find_cut, find_part};
use crate::graph::{GroupRef, LoadError, ProductRecord, ProductStatus};
use crate::resolve::{
    Pick, Picks, Refusal, Resolution, VariantIndex, check_picks, resolve, resolve_indexed, variant_matches,
};
use crate::source::ProductContent;

/// Option group kinds that only change a material (metal, gem material and color, finish), for a
/// record that gives a group no `geometry_role`. The `geometry_key` follows the roles alone.
pub const LOOK_ONLY_KINDS: &[&str] = &[
    "metal",
    "material",
    "gem",
    "gem_material",
    "gem_colour",
    "gem_color",
    "colour",
    "color",
    "anodize",
    "anodization",
    "finish",
    "plating",
];

/// Every runtime refusal code (agent error codes of `configurator.*`), with what it means.
pub const RUNTIME_REASONS: &[(&str, &str)] = &[
    (
        "configurator_not_open",
        "No product is open (configurator.open_product first).",
    ),
    (
        "configurator_bad_document",
        "A document is not valid JSON or misses a field the runtime needs.",
    ),
    (
        "configurator_missing_file",
        "A document the request names is not in the content source.",
    ),
    (
        "configurator_bad_path",
        "A path is absolute, has `..`, `.`, an empty or dots-only segment, a backslash, `:`, a control character or a device name, or is over-long; or `root` is empty, has a control character or is over-long.",
    ),
    ("configurator_io", "A document exists but could not be read."),
    (
        "configurator_contract",
        "A document breaks its owner's published schema or Regia's contract checks.",
    ),
    (
        "configurator_house_not_allowed",
        "The tenant may not load that house (a house tenant loads its own house and shared parts).",
    ),
    (
        "configurator_not_modelled",
        "The product is basic hardware, which is never modelled as a product.",
    ),
    (
        "configurator_duplicate",
        "The listing duplicates another; open the canonical listing.",
    ),
    (
        "configurator_not_orderable",
        "The product has no combination the snapshot sells.",
    ),
    (
        "configurator_unknown_group",
        "The product has no option group of that name.",
    ),
    (
        "configurator_unknown_value",
        "The option group has no value with that label.",
    ),
    (
        "configurator_value_unavailable",
        "The supplier no longer offers that value.",
    ),
    (
        "configurator_pick_shape",
        "A multi group takes a list of distinct labels; any other group takes one label.",
    ),
    (
        "configurator_not_sold",
        "No combination the snapshot sells contains that pick with the current picks (strict mode), or at all.",
    ),
    (
        "configurator_pick_required",
        "An axis or a required add-on cannot be cleared, only changed.",
    ),
    (
        "configurator_no_recipe",
        "The product has no recipe, so no build can be written (resolve still works).",
    ),
    (
        "configurator_unknown_wear",
        "The recipe has no wear preset with that id.",
    ),
    (
        "configurator_wrong_product",
        "The saved build is for another product.",
    ),
    (
        "configurator_build_invalid",
        "The build did not pass the jewelry.build/1 contract (an engine bug; report it).",
    ),
    (
        "configurator_bad_options",
        "A build option or tenant field the caller passed (studio_id, content_build_id, build_id, tenant_id) breaks the jewelry.build/1 contract; fix the caller's value.",
    ),
];

/// A configurator refusal: a code from [`RUNTIME_REASONS`] and a sentence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConfigError {
    /// The code.
    pub reason: &'static str,
    /// What happened.
    pub message: String,
}

impl ConfigError {
    fn new(reason: &'static str, message: impl Into<String>) -> Self {
        Self {
            reason,
            message: message.into(),
        }
    }
}

impl From<LoadError> for ConfigError {
    fn from(e: LoadError) -> Self {
        Self {
            reason: e.reason,
            message: e.message,
        }
    }
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.reason, self.message)
    }
}

impl std::error::Error for ConfigError {}

/// Who the configurator works for (`interfaces.md` § 0.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tenant {
    /// `house:<slug>` or `shop:<slug>`.
    pub tenant_id: String,
    /// For a shop: the houses it resells.
    pub houses: Vec<String>,
}

impl Tenant {
    /// A tenant; `houses` is used for `shop:` tenants only.
    pub fn new(tenant_id: impl Into<String>, houses: Vec<String>) -> Self {
        Self {
            tenant_id: tenant_id.into(),
            houses,
        }
    }

    /// Whether the tenant may load `house` (always `shared`).
    pub fn allows(&self, house: &str) -> bool {
        if house == "shared" {
            return true;
        }
        if let Some(own) = self.tenant_id.strip_prefix("house:") {
            return own == house;
        }
        self.tenant_id.starts_with("shop:") && self.houses.iter().any(|h| h == house)
    }
}

/// How [`Configurator::set_pick`] treats a value that conflicts with the other picks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SetMode {
    /// Refuse it (`configurator_not_sold`).
    #[default]
    Strict,
    /// Keep it and drop the conflicting picks.
    Adjust,
}

/// What one accepted pick changed.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct PickOutcome {
    /// Picks dropped because they conflicted with the new one (adjust mode): group -> old pick.
    pub dropped: BTreeMap<String, Pick>,
    /// Groups filled from the first sold combination: group -> new pick.
    pub filled: BTreeMap<String, Pick>,
}

/// The wear state of a build.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Wear {
    /// The recipe's wear preset.
    pub preset_id: String,
    /// Rotation in degrees.
    pub rotation_deg: [f64; 3],
    /// Mirrored (a left/right pair).
    pub mirror: bool,
}

impl Wear {
    /// The wear as a build stores it: each rotation within ±360° (a larger turn wraps, a
    /// non-finite one is 0), the `jewelry.build/1` bound. A recipe preset or a saved build with a
    /// turn of 400° used to fail every later build's contract check.
    pub fn normalized(&self) -> Self {
        let mut w = self.clone();
        for r in &mut w.rotation_deg {
            *r = if !r.is_finite() {
                0.0
            } else if r.abs() > 360.0 {
                *r % 360.0
            } else {
                *r
            };
        }
        w
    }
}

/// Whether `id` is a wear preset id the build schema accepts (`^[a-z0-9][a-z0-9_.-]{0,63}$`).
fn valid_preset_id(id: &str) -> bool {
    let b = id.as_bytes();
    (1..=64).contains(&b.len())
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'_' | b'.' | b'-'))
}

impl Default for Wear {
    fn default() -> Self {
        Self {
            preset_id: "default".to_owned(),
            rotation_deg: [0.0; 3],
            mirror: false,
        }
    }
}

/// The view state of a build (`jewelry.build/1` `camera`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Camera {
    /// `front`, `pan`, `tilt` or `spin`.
    pub rig_preset: String,
    /// Pan in mm.
    pub pan_mm: [f64; 2],
    /// Angle between the view and the front axis.
    pub tilt_deg: f64,
    /// Direction of the tilt around the front axis (0 = up).
    #[serde(default)]
    pub tilt_azimuth_deg: f64,
    /// Turn about the front axis.
    pub spin_deg: f64,
    /// Zoom factor.
    pub zoom: f64,
    /// Lens focal length.
    pub focal_length_mm: f64,
    /// Sensor width.
    pub sensor_mm: f64,
    /// Focus distance.
    pub focus_distance_mm: f64,
    /// Aperture.
    pub f_stop: f64,
    /// Depth of field on.
    pub dof: bool,
}

impl Camera {
    /// The front view at the recipe's default camera (60 mm, 100 mm lens, f/11 when the recipe
    /// says nothing); no depth of field (the viewer has none).
    pub fn default_for(recipe: Option<&Recipe>) -> Self {
        let d = recipe.and_then(|r| r.camera_default.as_ref());
        Self {
            rig_preset: "front".to_owned(),
            pan_mm: [0.0, 0.0],
            tilt_deg: 0.0,
            tilt_azimuth_deg: 0.0,
            spin_deg: 0.0,
            zoom: 1.0,
            focal_length_mm: d.and_then(|d| d.focal_length_mm).unwrap_or(100.0),
            sensor_mm: 36.0,
            focus_distance_mm: d.and_then(|d| d.distance_mm).unwrap_or(60.0),
            f_stop: d.and_then(|d| d.f_stop).unwrap_or(11.0),
            dof: false,
        }
    }

    /// Clamp to the view range (binding: `interfaces.md` § 3): tilt within `tilt_max_deg`; spin 0
    /// for `none`, within `rock_max_deg` for `rock`, any turn otherwise; pan within its bounds;
    /// zoom within 0.1..=100. A non-finite tilt, spin, pan, zoom, focal length, sensor width,
    /// focus distance or f-stop resets to the front view. Pan and the lens also stay inside the
    /// `jewelry.build/1` camera bounds (pan within ±1000 mm, focal length up to 2000 mm, sensor up to
    /// 200 mm, f-stop up to 256): a lens number that is not above zero resets to the default, a
    /// larger one clamps.
    pub fn clamped(&self, range: &ViewRange) -> Self {
        let fin = |x: f64, d: f64| if x.is_finite() { x } else { d };
        let mut c = self.clone();
        if !matches!(c.rig_preset.as_str(), "front" | "pan" | "tilt" | "spin") {
            c.rig_preset = "front".to_owned();
        }
        c.tilt_deg = fin(c.tilt_deg, 0.0).clamp(0.0, range.tilt_max_deg.clamp(0.0, 90.0));
        c.tilt_azimuth_deg = fin(c.tilt_azimuth_deg, 0.0).clamp(-180.0, 180.0);
        c.spin_deg = match range.spin.as_str() {
            "rock" => {
                let m = range.rock_max_deg.unwrap_or(0.0).clamp(0.0, 360.0);
                fin(c.spin_deg, 0.0).clamp(-m, m)
            }
            "about_front_axis" | "free" => fin(c.spin_deg, 0.0) % 360.0,
            _ => 0.0,
        };
        for (axis, p) in c.pan_mm.iter_mut().enumerate() {
            let v = fin(*p, 0.0);
            *p = match range.pan_bounds_mm {
                Some(b) if b[axis][0] <= b[axis][1] => v.clamp(b[axis][0], b[axis][1]),
                _ => v,
            }
            // The build schema's bound holds whatever the recipe's pan bounds say.
            .clamp(-1000.0, 1000.0);
        }
        c.zoom = fin(c.zoom, 1.0).clamp(0.1, 100.0);
        // The lens numbers are not bounded by the view range. A non-finite one would sit in the
        // saved camera and make every later build fail to serialize, and one outside the build
        // schema's bounds (a focal length of 0, an f-stop of 300) would fail every later build's
        // contract check, so a number that is not above zero resets and a larger one clamps.
        let lens = |x: f64, d: f64, max: f64| if x.is_finite() && x > 0.0 { x.min(max) } else { d };
        c.focal_length_mm = lens(c.focal_length_mm, 100.0, 2000.0);
        c.sensor_mm = lens(c.sensor_mm, 36.0, 200.0);
        c.focus_distance_mm = lens(c.focus_distance_mm, 60.0, f64::MAX);
        c.f_stop = lens(c.f_stop, 11.0, 256.0);
        c
    }
}

/// One value in [`OptionsView`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ValueView {
    /// The label (what a pick names).
    pub label: String,
    /// `<group>=<label>`.
    pub value_key: String,
    /// Still offered by the supplier.
    pub available: bool,
    /// Some sold combination contains it together with the other current picks.
    pub selectable: bool,
    /// Currently picked.
    pub picked: bool,
    /// The material it maps to, when the group drives a material zone.
    pub material_id: Option<String>,
    /// Why it is not selectable: `unavailable` (no longer offered), `not_sold_with` (a sold
    /// combination has it, but not with the picks in `conflicts`) or `not_orderable` (no sold
    /// combination has it at all). `None` when selectable.
    pub reason: Option<&'static str>,
    /// The current picks an adjust-mode pick of this value would change (`not_sold_with` only).
    pub conflicts: Vec<String>,
}

/// One option group in [`OptionsView`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GroupView {
    /// The group name.
    pub group: String,
    /// The group kind.
    pub kind: String,
    /// `axis` or `addon`.
    pub role: &'static str,
    /// An axis, or a required add-on.
    pub required: bool,
    /// Takes a list.
    pub multi: bool,
    /// Applies to the current variant (an add-on that does not is hidden by a viewer).
    pub applies: bool,
    /// Changes only a material (left out of the geometry key).
    pub look_only: bool,
    /// Changes nothing visible (the recipe's `no_visual`).
    pub no_visual: bool,
    /// The current pick.
    pub picked: Option<Pick>,
    /// The values.
    pub values: Vec<ValueView>,
}

/// Every option group of the open product.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OptionsView {
    /// The product key.
    pub product_key: String,
    /// The title.
    pub title: String,
    /// `complete`, or `incomplete` when the listing is known to be partial (a truncated variation
    /// list): such a listing is never presented as complete.
    pub listing: &'static str,
    /// The record's gap and flag codes.
    pub flags: Vec<String>,
    /// The groups, axes first, in record order.
    pub groups: Vec<GroupView>,
}

/// One material zone of a [`Look`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ZoneLook {
    /// `metal`, `gem`, ...
    pub kind: String,
    /// The material library entry, when one maps.
    pub material_id: Option<String>,
    /// Finish entries layered on the zone (multi finish groups).
    pub finishes: Vec<String>,
}

/// One part of a [`Look`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PartLook {
    /// The recipe instance.
    pub instance_id: String,
    /// The part, after any swap.
    pub part_ref: String,
    /// The kit part's kind, when a loaded kit has it.
    pub kind: Option<String>,
    /// The kit part's glTF file and node.
    pub glb: Option<String>,
    /// The node in the glTF file.
    pub node: Option<String>,
}

/// One gem slot of a [`Look`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GemLook {
    /// The slot.
    pub slot_id: String,
    /// Its role.
    pub role: String,
    /// The cut type.
    pub cut: String,
    /// The size in mm, when every `$value` resolves.
    pub size_mm: Option<Vec<f64>>,
    /// Stones in the slot.
    pub count: u32,
    /// The kit cut of that type and size, when a loaded kit has it.
    pub cut_id: Option<String>,
    /// The stone's material.
    pub material_id: Option<String>,
    /// Where the slot's stones sit on the front photo, `[x, y]` in `0..=1` from the top left: the
    /// option graph slot of the same id's `positions_norm` (1.2.0), `None` when the record has none.
    pub positions_norm: Option<Vec<[f64; 2]>>,
}

/// What the current picks look like: render-independent, for a viewer to draw.
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct Look {
    /// Zone -> material.
    pub zones: BTreeMap<String, ZoneLook>,
    /// The parts.
    pub parts: Vec<PartLook>,
    /// The gem slots.
    pub gems: Vec<GemLook>,
    /// The recipe's named values after the picks.
    pub values: BTreeMap<String, f64>,
    /// `group=label` picks the recipe drives but nothing maps (no material, no map entry), and
    /// parts or cuts no loaded kit has; a viewer shows its fallback for them.
    pub unmapped: Vec<String>,
}

/// What [`Configurator::build`] needs besides the picks.
#[derive(Debug, Clone)]
pub struct BuildOptions {
    /// The view; clamped to the recipe's view range. Default: the recipe's front view.
    pub camera: Option<Camera>,
    /// The wear. Default: the current wear.
    pub wear: Option<Wear>,
    /// The studio (lighting set) id.
    pub studio_id: String,
    /// `srgb` or `p3`.
    pub output_gamut: String,
    /// The content build the viewer runs.
    pub content_build_id: String,
    /// A fixed build id; default: derived from the build hash (the same build gets the same id).
    pub build_id: Option<String>,
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self {
            camera: None,
            wear: None,
            studio_id: "studio-default".to_owned(),
            output_gamut: "srgb".to_owned(),
            content_build_id: "unpackaged".to_owned(),
            build_id: None,
        }
    }
}

/// Whether `text` matches `^[a-z0-9][a-z0-9_.-]{0,63}$` (the build schema's `studio_id`): 1 to 64
/// ASCII characters.
fn is_studio_id(text: &str) -> bool {
    if text.len() > 64 {
        return false;
    }
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_lowercase() || first.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '.' | '-'))
}

/// Whether `text` matches `^(house|shop):[a-z0-9][a-z0-9-]{0,39}$` (the build schema's `tenant_id`):
/// a slug of 1 to 40 ASCII characters.
fn is_tenant_id(text: &str) -> bool {
    let Some(slug) = text.strip_prefix("house:").or_else(|| text.strip_prefix("shop:")) else {
        return false;
    };
    if slug.len() > 40 {
        return false;
    }
    let mut chars = slug.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_lowercase() || first.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Whether `text` is a lowercase UUID (`^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$`,
/// the build schema's `build_id`).
fn is_build_id(text: &str) -> bool {
    let hex = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c));
    let mut parts = text.split('-');
    matches!(
        (
            parts.next().map(|p| hex(p) && p.len() == 8),
            parts.next().map(|p| hex(p) && p.len() == 4),
            parts.next().map(|p| hex(p) && p.len() == 4),
            parts.next().map(|p| hex(p) && p.len() == 4),
            parts.next().map(|p| hex(p) && p.len() == 12),
            parts.next(),
        ),
        (Some(true), Some(true), Some(true), Some(true), Some(true), None)
    )
}

/// The host/data fields of a build, checked before the build JSON is composed: a value the
/// caller passed must be refused as `configurator_bad_options`, never `configurator_build_invalid`
/// (which means an engine bug).
fn check_build_options(options: &BuildOptions, tenant_id: &str) -> Result<(), ConfigError> {
    let bad = |field: &str, value: &str, rule: &str| {
        ConfigError::new(
            "configurator_bad_options",
            format!("{field} `{value}` breaks the jewelry.build/1 contract ({rule})"),
        )
    };
    if !is_studio_id(&options.studio_id) {
        return Err(bad(
            "studio_id",
            &options.studio_id,
            "^[a-z0-9][a-z0-9_.-]{0,63}$",
        ));
    }
    if options.content_build_id.is_empty() || options.content_build_id.len() > 128 {
        return Err(bad(
            "content_build_id",
            &options.content_build_id,
            "1 to 128 characters",
        ));
    }
    if !matches!(options.output_gamut.as_str(), "srgb" | "p3") {
        return Err(bad("output_gamut", &options.output_gamut, "`srgb` or `p3`"));
    }
    if let Some(id) = &options.build_id
        && !is_build_id(id)
    {
        return Err(bad(
            "build_id",
            id,
            "^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$",
        ));
    }
    if !is_tenant_id(tenant_id) {
        return Err(bad(
            "tenant_id",
            tenant_id,
            "^(house|shop):[a-z0-9][a-z0-9-]{0,39}$",
        ));
    }
    Ok(())
}

/// What reopening a saved build found.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReopenReport {
    /// The saved build said orderable.
    pub was_orderable: bool,
    /// It still resolves against the current snapshot ("no longer orderable" when false).
    pub orderable_now: bool,
    /// The snapshot moved since the build was saved.
    pub snapshot_changed: bool,
    /// The recipe changed since the build was saved.
    pub recipe_changed: bool,
    /// The resolution against the current snapshot.
    pub resolution: Value,
}

/// One open product and its current build.
#[derive(Debug, Clone)]
pub struct Configurator {
    content: ProductContent,
    tenant: Tenant,
    picks: Picks,
    wear: Wear,
    camera: Camera,
    /// Per variant: resolves to an orderable build with its own axis values and the first
    /// acceptable value of each required add-on.
    sellable: Vec<bool>,
}

impl Configurator {
    /// Open a product for `tenant` and start from its first sold combination.
    pub fn open(content: ProductContent, tenant: Tenant) -> Result<Self, ConfigError> {
        let product = &content.product;
        if !tenant.allows(product.house()) {
            return Err(ConfigError::new(
                "configurator_house_not_allowed",
                format!("`{}` may not load house `{}`", tenant.tenant_id, product.house()),
            ));
        }
        match product.status() {
            ProductStatus::Basic => {
                return Err(ConfigError::new(
                    "configurator_not_modelled",
                    format!("`{}` is basic hardware", product.product_key),
                ));
            }
            ProductStatus::Duplicate => {
                return Err(ConfigError::new(
                    "configurator_duplicate",
                    format!(
                        "`{}` duplicates `{}`",
                        product.product_key,
                        product.canonical_of.as_deref().unwrap_or("another listing")
                    ),
                ));
            }
            _ => {}
        }
        // One index for every variant's resolve: a scan per variant made opening O(variants²).
        let index = VariantIndex::new(product);
        let sellable: Vec<bool> = product
            .variants
            .iter()
            .map(|v| {
                product.variant_sellable(v)
                    && resolve_indexed(product, &first_picks(product, v), Some(&index)).orderable
            })
            .collect();
        if !sellable.iter().any(|s| *s) {
            return Err(ConfigError::new(
                "configurator_not_orderable",
                format!("`{}` has no combination the snapshot sells", product.product_key),
            ));
        }
        let wear = default_wear(content.recipe.as_ref());
        let camera = Camera::default_for(content.recipe.as_ref());
        let mut me = Self {
            content,
            tenant,
            picks: Picks::new(),
            wear,
            camera,
            sellable,
        };
        me.complete();
        Ok(me)
    }

    /// The loaded documents.
    pub fn content(&self) -> &ProductContent {
        &self.content
    }

    /// The option graph record.
    pub fn product(&self) -> &ProductRecord {
        &self.content.product
    }

    /// The current picks.
    pub fn picks(&self) -> &Picks {
        &self.picks
    }

    /// The tenant.
    pub fn tenant(&self) -> &Tenant {
        &self.tenant
    }

    /// The current camera (clamped).
    pub fn camera(&self) -> &Camera {
        &self.camera
    }

    /// The view range (the recipe's, else the operator defaults).
    pub fn view_range(&self) -> ViewRange {
        self.content
            .recipe
            .as_ref()
            .map(|r| r.view_range.clone())
            .unwrap_or_default()
    }

    /// Set the view; clamped to the view range. Returns the clamped camera.
    pub fn set_camera(&mut self, camera: Camera) -> &Camera {
        self.camera = camera.clamped(&self.view_range());
        &self.camera
    }

    /// Pick a recipe wear preset.
    pub fn set_wear(&mut self, preset_id: &str) -> Result<&Wear, ConfigError> {
        let wear = wear_preset(self.content.recipe.as_ref(), preset_id).ok_or_else(|| {
            ConfigError::new(
                "configurator_unknown_wear",
                format!("the recipe has no wear preset `{preset_id}`"),
            )
        })?;
        self.wear = wear;
        Ok(&self.wear)
    }

    /// Resolve the current picks.
    pub fn resolve(&self) -> Resolution {
        resolve(&self.content.product, &self.picks)
    }

    /// Every pick known, available, of the right shape, and (for an add-on) acceptable to the cart.
    fn picks_valid(&self, picks: &Picks) -> Result<(), Refusal> {
        let product = &self.content.product;
        if let Some(first) = check_picks(product, picks).into_iter().next() {
            return Err(first);
        }
        for (group, pick) in picks {
            if let Some(GroupRef::Addon(i)) = product.group(group) {
                for label in pick.labels() {
                    let ok = product.addons[i]
                        .values
                        .iter()
                        .find(|v| v.label == label)
                        .is_some_and(|v| product.addon_value_acceptable(v));
                    if !ok {
                        return Err(Refusal {
                            code: "no_cart_code",
                            group: Some(group.clone()),
                            value: Some(label.to_owned()),
                            message: format!("`{group}={label}` cannot go in the cart"),
                        });
                    }
                }
            }
        }
        Ok(())
    }

    /// Whether variant `i` is sellable, matches the axis picks, and every picked add-on applies to it.
    fn fits(&self, i: usize, picks: &Picks) -> bool {
        let product = &self.content.product;
        let v = &product.variants[i];
        self.sellable[i]
            && variant_matches(product, v, picks)
            && picks.iter().all(|(g, p)| match product.group(g) {
                Some(GroupRef::Addon(a)) => p.is_empty() || product.addons[a].applies(&v.variant_key),
                _ => true,
            })
    }

    /// Whether some sold combination contains every pick in `picks` (a partial build): every
    /// label known, available and of the right shape, every picked add-on value acceptable to the
    /// cart, and a sellable variant that matches the axis picks and that every picked add-on
    /// applies to. `Ok` is the first such variant's index; `Err` carries the first refusal.
    pub fn consistent(&self, picks: &Picks) -> Result<usize, Refusal> {
        self.picks_valid(picks)?;
        (0..self.content.product.variants.len())
            .find(|&i| self.fits(i, picks))
            .ok_or_else(|| Refusal {
                code: "not_offered",
                group: None,
                value: None,
                message: "no combination the snapshot sells has these picks".to_owned(),
            })
    }

    /// The sold build that keeps `group = value` and changes the fewest other current picks: the
    /// sellable variant (ties: the option graph's variant order) whose conflicting picks are
    /// fewest. The search visits the explicit variant list once; it never enumerates
    /// combinations. `None` when no sold combination has the value at all.
    fn adjust_plan(&self, group: &str, value: &Pick) -> Option<(usize, Vec<String>)> {
        let alone: Picks = std::iter::once((group.to_owned(), value.clone())).collect();
        self.consistent(&alone).ok()?;
        let product = &self.content.product;
        // The other current picks, each with whether it is valid on its own (independent of the variant).
        let others: Vec<(&String, &Pick, bool)> = self
            .picks
            .iter()
            .filter(|(g, p)| g.as_str() != group && !p.is_empty())
            .map(|(g, p)| {
                let one: Picks = std::iter::once((g.clone(), p.clone())).collect();
                (g, p, self.picks_valid(&one).is_ok())
            })
            .collect();
        let mut best: Option<(usize, Vec<String>)> = None;
        for i in 0..product.variants.len() {
            if !self.fits(i, &alone) {
                continue;
            }
            let v = &product.variants[i];
            let changed: Vec<String> = others
                .iter()
                .filter(|(g, p, valid)| {
                    let kept = *valid
                        && match (product.group(g), p) {
                            (Some(GroupRef::Axis(_)), Pick::One(l)) => v.picks.get(*g) == Some(l),
                            (Some(GroupRef::Addon(a)), _) => product.addons[a].applies(&v.variant_key),
                            _ => false,
                        };
                    !kept
                })
                .map(|(g, _, _)| (*g).clone())
                .collect();
            if best.as_ref().is_none_or(|(_, b)| changed.len() < b.len()) {
                let done = changed.is_empty();
                best = Some((i, changed));
                if done {
                    break;
                }
            }
        }
        best
    }

    /// Fill every open axis and required add-on from the first sold combination that contains
    /// the current picks; returns what was filled. A no-op when the picks are inconsistent.
    fn complete(&mut self) -> BTreeMap<String, Pick> {
        self.complete_with(None)
    }

    /// [`Configurator::complete`] from variant `prefer` when it fits the picks.
    fn complete_with(&mut self, prefer: Option<usize>) -> BTreeMap<String, Pick> {
        let mut filled = BTreeMap::new();
        let at = match prefer.filter(|&i| self.picks_valid(&self.picks).is_ok() && self.fits(i, &self.picks))
        {
            Some(i) => i,
            None => match self.consistent(&self.picks) {
                Ok(i) => i,
                Err(_) => return filled,
            },
        };
        let product = &self.content.product;
        let variant = &product.variants[at];
        for (group, label) in &variant.picks {
            let pick = Pick::One(label.clone());
            if self.picks.get(group).is_none_or(Pick::is_empty) {
                self.picks.insert(group.clone(), pick.clone());
                filled.insert(group.clone(), pick);
            }
        }
        for addon in &product.addons {
            if addon.required
                && addon.applies(&variant.variant_key)
                && self.picks.get(&addon.group).is_none_or(Pick::is_empty)
                && let Some(v) = addon.values.iter().find(|v| product.addon_value_acceptable(v))
            {
                let pick = if addon.multi {
                    Pick::Many(vec![v.label.clone()])
                } else {
                    Pick::One(v.label.clone())
                };
                self.picks.insert(addon.group.clone(), pick.clone());
                filled.insert(addon.group.clone(), pick);
            }
        }
        filled
    }

    /// Apply one pick (module docs). `None` clears an optional add-on.
    pub fn set_pick(
        &mut self,
        group: &str,
        value: Option<Pick>,
        mode: SetMode,
    ) -> Result<PickOutcome, ConfigError> {
        let product = &self.content.product;
        // Names and labels match ignoring ASCII case; the state holds the record's spelling.
        let Some(at) = product.group_folded(group) else {
            return Err(ConfigError::new(
                "configurator_unknown_group",
                format!("`{}` has no option group `{group}`", product.product_key),
            ));
        };
        let (group, required) = match at {
            GroupRef::Axis(i) => (product.axes[i].group.clone(), true),
            GroupRef::Addon(i) => (product.addons[i].group.clone(), product.addons[i].required),
        };
        let group = group.as_str();
        let value = value.map(|v| product.canonical_pick(at, &v));
        let value = match value {
            Some(v) if !v.is_empty() => v,
            _ if required => {
                return Err(ConfigError::new(
                    "configurator_pick_required",
                    format!("`{group}` must keep a value; pick another one instead"),
                ));
            }
            _ => {
                self.picks.remove(group);
                let filled = self.complete();
                return Ok(PickOutcome {
                    dropped: BTreeMap::new(),
                    filled,
                });
            }
        };
        let mut next = self.picks.clone();
        next.insert(group.to_owned(), value.clone());
        let mut dropped = BTreeMap::new();
        let mut prefer = None;
        if let Err(refusal) = self.consistent(&next) {
            let alone: Picks = std::iter::once((group.to_owned(), value.clone())).collect();
            if let Err(alone_refusal) = self.consistent(&alone) {
                return Err(refusal_error(&alone_refusal));
            }
            if mode == SetMode::Strict {
                return Err(ConfigError::new(
                    "configurator_not_sold",
                    format!(
                        "no sold combination has `{group}` = {} with the current picks ({}); \
                         use adjust mode to drop the conflicting picks",
                        pick_text(&value),
                        refusal.message
                    ),
                ));
            }
            // Keep the new value and change the fewest other picks.
            let Some((at_variant, changed)) = self.adjust_plan(group, &value) else {
                return Err(ConfigError::new(
                    "configurator_not_sold",
                    format!("no sold combination has `{group}` = {}", pick_text(&value)),
                ));
            };
            for g in changed {
                if let Some(old) = next.remove(&g) {
                    dropped.insert(g, old);
                }
            }
            prefer = Some(at_variant);
        }
        self.picks = next;
        let filled = self.complete_with(prefer);
        Ok(PickOutcome { dropped, filled })
    }

    /// Every option group and value of the product (module docs).
    pub fn options(&self) -> OptionsView {
        let product = &self.content.product;
        let current = self.consistent(&self.picks).ok().map(|i| &product.variants[i]);
        let no_visual: BTreeSet<&str> = self
            .content
            .recipe
            .as_ref()
            .map(|r| r.no_visual.iter().map(String::as_str).collect())
            .unwrap_or_default();
        let mut groups = Vec::new();
        let mut push = |group: &str,
                        kind: &str,
                        role: &'static str,
                        required: bool,
                        multi: bool,
                        applies: bool,
                        values: &[crate::graph::OptionValue],
                        gem_values: bool| {
            let picked = self.picks.get(group).cloned();
            let values = values
                .iter()
                .map(|v| {
                    let mut trial = self.picks.clone();
                    let pick = if multi {
                        let mut list = match self.picks.get(group) {
                            Some(Pick::Many(l)) => l.clone(),
                            _ => Vec::new(),
                        };
                        if !list.contains(&v.label) {
                            list.push(v.label.clone());
                        }
                        Pick::Many(list)
                    } else {
                        Pick::One(v.label.clone())
                    };
                    trial.insert(group.to_owned(), pick.clone());
                    let selectable = self.consistent(&trial).is_ok();
                    let (reason, conflicts) = if selectable {
                        (None, Vec::new())
                    } else if !v.available {
                        (Some("unavailable"), Vec::new())
                    } else {
                        match self.adjust_plan(group, &pick) {
                            Some((_, changed)) => (Some("not_sold_with"), changed),
                            None => (Some("not_orderable"), Vec::new()),
                        }
                    };
                    let is_picked = picked
                        .as_ref()
                        .is_some_and(|p| p.labels().contains(&v.label.as_str()));
                    ValueView {
                        label: v.label.clone(),
                        value_key: if v.value_key.is_empty() {
                            format!("{group}={}", v.label)
                        } else {
                            v.value_key.clone()
                        },
                        available: v.available,
                        selectable,
                        picked: is_picked,
                        material_id: self
                            .material_for(group, &v.label, if gem_values { v.gem.as_ref() } else { None })
                            .map(|m| m.material_id.clone()),
                        reason,
                        conflicts,
                    }
                })
                .collect();
            groups.push(GroupView {
                group: group.to_owned(),
                kind: kind.to_owned(),
                role,
                required,
                multi,
                applies,
                look_only: self.look_only(group),
                no_visual: no_visual.contains(group),
                picked,
                values,
            });
        };
        for axis in &product.axes {
            push(
                &axis.group,
                &axis.kind,
                "axis",
                true,
                false,
                true,
                &axis.values,
                false,
            );
        }
        for addon in &product.addons {
            let applies = current.is_none_or(|v| addon.applies(&v.variant_key));
            push(
                &addon.group,
                &addon.kind,
                "addon",
                addon.required,
                addon.multi,
                applies,
                &addon.values,
                true,
            );
        }
        OptionsView {
            product_key: product.product_key.clone(),
            title: product.title.clone(),
            listing: if product.is_incomplete() {
                "incomplete"
            } else {
                "complete"
            },
            flags: product.gap_codes(),
            groups,
        }
    }

    /// The material a value maps to, when the recipe drives a zone with its group.
    fn material_for(&self, group: &str, label: &str, gem: Option<&Value>) -> Option<&MaterialEntry> {
        let recipe = self.content.recipe.as_ref()?;
        let lib = self.content.library.as_ref()?;
        recipe.drives_of(group).find(|d| d.to.material_zone.is_some())?;
        lib.lookup(group, label, gem)
    }

    /// Whether a group only changes a material. A record's `geometry_role` says so when it is
    /// `excluded` or `geometry`; otherwise its kind is in [`LOOK_ONLY_KINDS`], or every recipe drive
    /// of it targets a material zone.
    pub fn look_only(&self, group: &str) -> bool {
        match self.content.product.group_role(group) {
            Some("excluded") => return true,
            Some("geometry") => return false,
            _ => {}
        }
        if LOOK_ONLY_KINDS.contains(&self.content.product.group_kind(group)) {
            return true;
        }
        let Some(recipe) = &self.content.recipe else {
            return false;
        };
        let mut drives = recipe.drives_of(group).peekable();
        drives.peek().is_some()
            && drives
                .all(|d| d.to.material_zone.is_some() && d.to.value.is_none() && d.to.part_swap.is_none())
    }

    /// The `geometry_key` of the current picks ([`crate::resolve::geometry_key`]): the resolution's
    /// key, so 3D assets are keyed the same in every implementation. A build that no longer resolves
    /// (reopened after the snapshot moved) has none, so its picks are keyed as they stand.
    pub fn geometry_key(&self) -> String {
        let product = &self.content.product;
        self.resolve()
            .geometry_key
            .unwrap_or_else(|| crate::resolve::geometry_key(product, &self.picks))
    }

    /// What the current picks look like (module docs).
    pub fn look(&self) -> Look {
        let mut look = Look::default();
        let Some(recipe) = &self.content.recipe else {
            return look;
        };
        let product = &self.content.product;
        for z in &recipe.material_zones {
            look.zones.insert(
                z.zone.clone(),
                ZoneLook {
                    kind: z.kind.clone(),
                    material_id: None,
                    finishes: Vec::new(),
                },
            );
        }
        look.values = recipe.values.clone();
        let mut part_refs: BTreeMap<String, String> = recipe
            .parts
            .iter()
            .map(|p| (p.instance_id.clone(), p.part_ref.clone()))
            .collect();
        for drive in &recipe.drives {
            let Some(pick) = self.picks.get(&drive.group) else {
                continue;
            };
            let at = product.group(&drive.group);
            for label in pick.labels() {
                let key = format!("{}={label}", drive.group);
                if let Some(zone) = &drive.to.material_zone {
                    let gem = match at {
                        Some(GroupRef::Addon(i)) => product.addons[i]
                            .values
                            .iter()
                            .find(|v| v.label == label)
                            .and_then(|v| v.gem.as_ref()),
                        _ => None,
                    };
                    let entry = self
                        .content
                        .library
                        .as_ref()
                        .and_then(|l| l.lookup(&drive.group, label, gem));
                    let z = look.zones.entry(zone.clone()).or_insert_with(|| ZoneLook {
                        kind: String::new(),
                        material_id: None,
                        finishes: Vec::new(),
                    });
                    match entry {
                        Some(m) if m.kind == "finish" => z.finishes.push(m.material_id.clone()),
                        Some(m) => z.material_id = Some(m.material_id.clone()),
                        None => look.unmapped.push(key.clone()),
                    }
                }
                if let Some(name) = &drive.to.value {
                    match drive.to.map.get(label).and_then(Value::as_f64) {
                        Some(x) => {
                            look.values.insert(name.clone(), x);
                        }
                        None => look.unmapped.push(key.clone()),
                    }
                }
                if let Some(instance) = &drive.to.part_swap {
                    match drive.to.map.get(label).and_then(Value::as_str) {
                        Some(r) if part_refs.contains_key(instance) => {
                            part_refs.insert(instance.clone(), r.to_owned());
                        }
                        _ => look.unmapped.push(key.clone()),
                    }
                }
            }
        }
        for p in &recipe.parts {
            let part_ref = part_refs
                .get(&p.instance_id)
                .cloned()
                .unwrap_or_else(|| p.part_ref.clone());
            let found = find_part(&self.content.kits, &part_ref);
            if found.is_none() && !self.content.kits.is_empty() {
                look.unmapped.push(format!("part:{part_ref}"));
            }
            look.parts.push(PartLook {
                instance_id: p.instance_id.clone(),
                kind: found.map(|(_, k)| k.kind.clone()),
                glb: found.and_then(|(_, k)| k.glb.clone()),
                node: found.and_then(|(_, k)| k.node.clone()),
                part_ref,
            });
        }
        for slot in &recipe.gem_slots {
            let size = recipe.slot_size(slot, &look.values);
            let cut = size
                .as_deref()
                .and_then(|s| find_cut(&self.content.kits, &slot.cut, s));
            if cut.is_none() && !self.content.kits.is_empty() {
                look.unmapped.push(format!("cut:{}:{}", slot.slot_id, slot.cut));
            }
            look.gems.push(GemLook {
                slot_id: slot.slot_id.clone(),
                role: slot.role.clone(),
                cut: slot.cut.clone(),
                size_mm: size,
                count: slot.count,
                cut_id: cut.map(|c| c.cut_id.clone()),
                material_id: slot
                    .material_slot
                    .as_ref()
                    .and_then(|z| look.zones.get(z))
                    .and_then(|z| z.material_id.clone()),
                positions_norm: product.slot(&slot.slot_id).and_then(|s| s.positions_norm.clone()),
            });
        }
        look
    }

    /// Write the `jewelry.build/1` JSON of the current picks (module docs).
    pub fn build(&self, options: &BuildOptions) -> Result<Value, ConfigError> {
        check_build_options(options, &self.tenant.tenant_id)?;
        let product = &self.content.product;
        let Some(recipe) = &self.content.recipe else {
            return Err(ConfigError::new(
                "configurator_no_recipe",
                format!("`{}` has no recipe", product.product_key),
            ));
        };
        let camera = options
            .camera
            .as_ref()
            .unwrap_or(&self.camera)
            .clamped(&recipe.view_range);
        let wear = options.wear.as_ref().unwrap_or(&self.wear).normalized();
        let camera_json = serde_json::to_value(&camera)
            .map_err(|e| ConfigError::new("configurator_build_invalid", e.to_string()))?;
        let wear_json = serde_json::to_value(&wear)
            .map_err(|e| ConfigError::new("configurator_build_invalid", e.to_string()))?;
        let picks: BTreeMap<&String, &Pick> = self.picks.iter().filter(|(_, p)| !p.is_empty()).collect();
        let picks_json = serde_json::to_value(&picks)
            .map_err(|e| ConfigError::new("configurator_build_invalid", e.to_string()))?;
        let build_hash = crate::checks::build_hash(
            &camera_json,
            &picks_json,
            &product.product_key,
            &recipe.hash,
            &product.snapshot_id,
            &wear_json,
        )
        .ok_or_else(|| {
            ConfigError::new(
                "configurator_build_invalid",
                "a number cannot be written canonically",
            )
        })?;
        let build_id = options
            .build_id
            .clone()
            .unwrap_or_else(|| id_from_hash(&build_hash));
        // spec 42 § 3.2: an incomplete listing carries the caveat in the build's resolution
        // reasons (next to any refusals); it is not a refusal — the build can be orderable.
        let mut resolution = self.resolve().to_build_json();
        if product.is_incomplete()
            && let Some(reasons) = resolution["reasons"].as_array_mut()
        {
            reasons.push(json!({ "code": crate::resolve::BUILD_CAVEAT_INCOMPLETE }));
        }
        let build = json!({
            "schema": "jewelry.build/1",
            "build_id": build_id,
            "build_hash": build_hash,
            "tenant_id": self.tenant.tenant_id,
            "house": product.house(),
            "product_key": product.product_key,
            "design_key": product.design_key(),
            "geometry_key": self.geometry_key(),
            "picks": picks_json,
            "resolution": resolution,
            "snapshot_id": product.snapshot_id,
            "recipe": {"design_key": recipe.design_key, "version": recipe.recipe_version, "hash": recipe.hash},
            "wear": wear_json,
            "camera": camera_json,
            "studio_id": options.studio_id,
            "tier": "interactive",
            "output_gamut": options.output_gamut,
            "front_still_ref": null,
            "engine_version": env!("CARGO_PKG_VERSION"),
            "content_build_id": options.content_build_id,
        });
        let mut build = build;
        if build_schema_has_top_level_sku() {
            // The v2 build schema repeats the resolution's SKU and variant at the top level.
            let r = &build["resolution"];
            let (sku, variant) = (r["sku"].clone(), r["variant_key"].clone());
            build["sku"] = sku;
            build["house_variant_id"] = variant;
        }
        crate::check(&build, Some(Interface::Build), None).map_err(|errors| {
            ConfigError::new(
                "configurator_build_invalid",
                errors
                    .iter()
                    .take(5)
                    .map(|e| format!("{} at `{}`: {}", e.reason, e.path, e.message))
                    .collect::<Vec<_>>()
                    .join("; "),
            )
        })?;
        Ok(build)
    }

    /// Reopen a saved build: take its picks, wear and camera as they are, and re-resolve against
    /// the current snapshot (`interfaces.md` § 7: a build that no longer resolves is marked "no
    /// longer orderable", not repaired).
    pub fn load_build(&mut self, build: &Value) -> Result<ReopenReport, ConfigError> {
        let bad = |m: String| ConfigError::new("configurator_bad_document", m);
        if build.get("schema").and_then(Value::as_str) != Some("jewelry.build/1") {
            return Err(bad("not a jewelry.build/1 document".to_owned()));
        }
        let product_key = build
            .get("product_key")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if product_key != self.content.product.product_key {
            return Err(ConfigError::new(
                "configurator_wrong_product",
                format!(
                    "the build is for `{product_key}`, `{}` is open",
                    self.content.product.product_key
                ),
            ));
        }
        let picks: Picks = serde_json::from_value(build.get("picks").cloned().unwrap_or(Value::Null))
            .map_err(|e| bad(format!("build picks: {e}")))?;
        let picks = self.content.product.canonical_picks(&picks);
        let wear: Wear = serde_json::from_value(build.get("wear").cloned().unwrap_or(Value::Null))
            .map_err(|e| bad(format!("build wear: {e}")))?;
        if !valid_preset_id(&wear.preset_id) {
            return Err(bad(format!(
                "build wear: `{}` is not a wear preset id",
                wear.preset_id.escape_debug()
            )));
        }
        let camera: Camera = serde_json::from_value(build.get("camera").cloned().unwrap_or(Value::Null))
            .map_err(|e| bad(format!("build camera: {e}")))?;
        self.picks = picks;
        self.wear = wear.normalized();
        self.camera = camera.clamped(&self.view_range());
        let resolution = self.resolve();
        let s = |p: &str| {
            build
                .pointer(p)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        Ok(ReopenReport {
            was_orderable: build
                .pointer("/resolution/orderable")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            orderable_now: resolution.orderable,
            snapshot_changed: s("/snapshot_id") != self.content.product.snapshot_id,
            recipe_changed: self
                .content
                .recipe
                .as_ref()
                .is_some_and(|r| s("/recipe/hash") != r.hash),
            resolution: resolution.to_json(),
        })
    }
}

/// Whether the compiled-in `jewelry.build/1` schema declares the optional top-level `sku` and
/// `house_variant_id` (the v2 contract drop does; ADR-2480's first draft did not).
fn build_schema_has_top_level_sku() -> bool {
    static HAS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *HAS.get_or_init(|| {
        Interface::Build
            .schema_text()
            .and_then(|t| serde_json::from_str::<Value>(t).ok())
            .is_some_and(|s| s.pointer("/properties/house_variant_id").is_some())
    })
}

fn refusal_error(r: &Refusal) -> ConfigError {
    let reason = match r.code {
        "unknown_group" => "configurator_unknown_group",
        "unknown_value" => "configurator_unknown_value",
        "value_unavailable" => "configurator_value_unavailable",
        "wrong_pick_type" | "duplicate_value" => "configurator_pick_shape",
        _ => "configurator_not_sold",
    };
    ConfigError::new(reason, r.message.clone())
}

fn pick_text(p: &Pick) -> String {
    match p {
        Pick::One(s) => format!("`{s}`"),
        Pick::Many(v) => format!("[{}]", v.join(", ")),
    }
}

/// The picks a variant resolves with: its own axis values and the first acceptable value of each
/// required add-on that applies to it.
fn first_picks(product: &ProductRecord, variant: &crate::graph::Variant) -> Picks {
    let mut picks: Picks = variant
        .picks
        .iter()
        .map(|(g, l)| (g.clone(), Pick::One(l.clone())))
        .collect();
    for addon in product
        .addons
        .iter()
        .filter(|a| a.required && a.applies(&variant.variant_key))
    {
        if let Some(v) = addon.values.iter().find(|v| product.addon_value_acceptable(v)) {
            let pick = if addon.multi {
                Pick::Many(vec![v.label.clone()])
            } else {
                Pick::One(v.label.clone())
            };
            picks.insert(addon.group.clone(), pick);
        }
    }
    picks
}

/// A recipe wear preset as a build stores it; `None` when the recipe has none of that id, or the id
/// is not one the build schema accepts (a build with it would fail its contract check).
fn wear_preset(recipe: Option<&Recipe>, preset_id: &str) -> Option<Wear> {
    if !valid_preset_id(preset_id) {
        return None;
    }
    let presets = recipe?.wear.as_ref()?.get("presets")?.as_array()?;
    let p = presets
        .iter()
        .find(|p| p.get("preset_id").and_then(Value::as_str) == Some(preset_id))?;
    let rot = p.get("rotation_deg").and_then(Value::as_array);
    let r = |i: usize| rot.and_then(|r| r.get(i)).and_then(Value::as_f64).unwrap_or(0.0);
    Some(
        Wear {
            preset_id: preset_id.to_owned(),
            rotation_deg: [r(0), r(1), r(2)],
            mirror: p.get("mirror").and_then(Value::as_bool).unwrap_or(false),
        }
        .normalized(),
    )
}

fn default_wear(recipe: Option<&Recipe>) -> Wear {
    let first = recipe
        .and_then(|r| r.wear.as_ref())
        .and_then(|w| w.get("presets"))
        .and_then(Value::as_array)
        .and_then(|p| p.first())
        .and_then(|p| p.get("preset_id"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    first.and_then(|id| wear_preset(recipe, &id)).unwrap_or_default()
}

/// A build id derived from the build hash: the first 32 hex digits in UUID form, with the version
/// nibble 8 (a name-based id) and the RFC 9562 variant bits.
fn id_from_hash(hash: &str) -> String {
    let hex: Vec<char> = hash.trim_start_matches("sha256:").chars().take(32).collect();
    let mut h: Vec<char> = hex;
    h.resize(32, '0');
    h[12] = '8';
    h[16] = match h[16].to_digit(16).unwrap_or(0) & 0x3 {
        0 => '8',
        1 => '9',
        2 => 'a',
        _ => 'b',
    };
    let s: String = h.into_iter().collect();
    format!(
        "{}-{}-{}-{}-{}",
        &s[0..8],
        &s[8..12],
        &s[12..16],
        &s[16..20],
        &s[20..32]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_from_hashes_look_like_uuids() {
        let id = id_from_hash("sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef");
        assert_eq!(id, "01234567-89ab-8def-8123-456789abcdef");
    }

    /// Audit fix (sprint-fix-audit-jw, finding 4): opening a product resolves every variant through
    /// one index, so it looks at no variant list per variant (it used to scan all of them each time).
    #[test]
    fn opening_a_product_does_not_scan_the_variants_per_variant() {
        let n = 400;
        let values: Vec<Value> = (0..n)
            .map(|i| json!({"value_key": format!("Size={i}"), "label": format!("S{i}"), "available": true}))
            .collect();
        let variants: Vec<Value> = (0..n)
            .map(|i| json!({"variant_key": format!("{}", 1000 + i), "sku": format!("K{i}"), "picks": {"Size": format!("S{i}")}}))
            .collect();
        let doc = json!({
            "schema": "jewelry.option_graph/1", "snapshot_id": "2026-11-04-000000000000",
            "product_key": "house-a/big", "title": "Big", "status": "orderable",
            "cart": {"platform": "test", "mode": "shopify_lines"},
            "axes": [{"group": "Size", "kind": "gem_size", "values": values}],
            "variants": variants, "addons": [],
        });
        let product = ProductRecord::from_value(doc).expect("record");
        let content = ProductContent {
            product,
            recipe: None,
            kits: Vec::new(),
            library: None,
            read: Vec::new(),
        };
        crate::resolve::VARIANT_SCANS.with(|c| c.set(0));
        let config = Configurator::open(content, Tenant::new("house:house-a", vec![])).expect("opens");
        let scans = crate::resolve::VARIANT_SCANS.with(std::cell::Cell::get);
        assert_eq!(scans, 0, "open scanned {scans} variants for {n} variants");
        assert!(config.sellable.iter().all(|s| *s));
        // A partial pick still scans, once.
        crate::resolve::VARIANT_SCANS.with(|c| c.set(0));
        let r = resolve(&config.content.product, &Picks::new());
        assert_eq!(r.open, ["Size"]);
        assert_eq!(crate::resolve::VARIANT_SCANS.with(std::cell::Cell::get), n);
    }

    #[test]
    fn tenants_see_their_houses_and_shared() {
        let h = Tenant::new("house:house-a", vec![]);
        assert!(h.allows("house-a") && h.allows("shared") && !h.allows("house-b"));
        let s = Tenant::new("shop:example-shop", vec!["house-b".to_owned()]);
        assert!(s.allows("house-b") && !s.allows("house-a"));
    }

    #[test]
    fn camera_clamps_to_the_view_range() {
        let range = ViewRange::default();
        let mut c = Camera::default_for(None);
        c.tilt_deg = 75.0;
        c.spin_deg = 40.0;
        c.zoom = f64::NAN;
        c.rig_preset = "orbit".to_owned();
        let k = c.clamped(&range);
        assert_eq!(
            (k.tilt_deg, k.spin_deg, k.zoom, k.rig_preset.as_str()),
            (30.0, 0.0, 1.0, "front")
        );
        let rock = ViewRange {
            spin: "rock".to_owned(),
            rock_max_deg: Some(15.0),
            pan_bounds_mm: Some([[-3.0, 3.0], [-2.0, 2.0]]),
            ..ViewRange::default()
        };
        c.spin_deg = -40.0;
        c.pan_mm = [9.0, -9.0];
        let k = c.clamped(&rock);
        assert_eq!((k.spin_deg, k.pan_mm), (-15.0, [3.0, -2.0]));
    }

    fn open_size_product() -> Configurator {
        let doc = json!({
            "schema": "jewelry.option_graph/1", "snapshot_id": "2026-11-04-000000000000",
            "product_key": "house-a/big", "title": "Big", "status": "orderable",
            "cart": {"platform": "test", "mode": "shopify_lines"},
            "axes": [{"group": "Size", "kind": "gem_size", "values": [
                {"value_key": "Size=S0", "label": "S0", "available": true},
                {"value_key": "Size=S1", "label": "S1", "available": true}
            ]}],
            "variants": [
                {"variant_key": "1000", "sku": "K0", "picks": {"Size": "S0"}},
                {"variant_key": "1001", "sku": "K1", "picks": {"Size": "S1"}}
            ],
            "addons": [],
        });
        let product = ProductRecord::from_value(doc).expect("record");
        Configurator::open(
            ProductContent {
                product,
                recipe: None,
                kits: Vec::new(),
                library: None,
                read: Vec::new(),
            },
            Tenant::new("house:house-a", vec![]),
        )
        .expect("opens")
    }

    fn saved_build(picks: Value) -> Value {
        json!({
            "schema": "jewelry.build/1",
            "product_key": "house-a/big",
            "picks": picks,
            "wear": {"preset_id": "default", "rotation_deg": [0.0, 0.0, 0.0], "mirror": false},
            "camera": {
                "rig_preset": "front", "pan_mm": [0.0, 0.0], "tilt_deg": 0.0, "spin_deg": 0.0,
                "zoom": 1.0, "focal_length_mm": 100.0, "sensor_mm": 36.0,
                "focus_distance_mm": 60.0, "f_stop": 11.0, "dof": false
            },
            "resolution": {"orderable": true},
            "snapshot_id": "2026-11-04-000000000000",
        })
    }

    /// `Size=S0` and `size=S1` are one group with two values. Keeping the later spelling used to
    /// reopen the build as orderable variant 1001.
    #[test]
    fn load_build_refuses_two_spellings_that_disagree() {
        let mut config = open_size_product();
        let report = config
            .load_build(&saved_build(json!({"Size": "S0", "size": "S1"})))
            .expect("reopens");
        assert!(!report.orderable_now, "{:?}", report.resolution);
        let codes: Vec<&str> = report.resolution["reasons"]
            .as_array()
            .expect("reasons")
            .iter()
            .filter_map(|r| r["code"].as_str())
            .collect();
        assert!(
            codes.contains(&"duplicate_value"),
            "{codes:?} {}",
            report.resolution
        );
    }

    /// The same value under two spellings stays one pick and still sells that variant.
    #[test]
    fn load_build_keeps_one_pick_when_spellings_agree() {
        let mut config = open_size_product();
        let report = config
            .load_build(&saved_build(json!({"Size": "S0", "SIZE": "s0"})))
            .expect("reopens");
        assert!(report.orderable_now, "{:?}", report.resolution);
        assert_eq!(report.resolution["variant_key"], json!("1000"));
        assert_eq!(report.resolution["sku"], json!("K0"));
    }

    /// w7-runtime-smalls-z item 1: the id checks enforce the `{0,63}` / `{0,39}` length bounds the
    /// schema patterns cite, so an over-long id is the caller's `configurator_bad_options`, not a
    /// later `configurator_build_invalid` from the build schema.
    #[test]
    fn studio_and_tenant_ids_keep_their_length_bounds() {
        let with_studio = |len: usize| BuildOptions {
            studio_id: "s".repeat(len),
            ..BuildOptions::default()
        };
        assert!(check_build_options(&with_studio(64), "house:house-a").is_ok());
        let err = check_build_options(&with_studio(65), "house:house-a").expect_err("65 chars");
        assert_eq!(err.reason, "configurator_bad_options", "{err}");
        let err = check_build_options(&with_studio(100), "house:house-a").expect_err("100 chars");
        assert_eq!(err.reason, "configurator_bad_options", "{err}");
        let options = BuildOptions::default();
        for prefix in ["house:", "shop:"] {
            let ok = format!("{prefix}{}", "t".repeat(40));
            assert!(check_build_options(&options, &ok).is_ok(), "{ok}");
            let long = format!("{prefix}{}", "t".repeat(41));
            let err = check_build_options(&options, &long).expect_err("41-char slug");
            assert_eq!(err.reason, "configurator_bad_options", "{err}");
        }
        // Through the public path too: the over-long studio id is refused before the recipe check.
        let config = open_size_product();
        let err = config.build(&with_studio(65)).expect_err("refused");
        assert_eq!(err.reason, "configurator_bad_options", "{err}");
    }

    /// A non-finite lens number must not stick in the camera: `build` serializes it, and serde
    /// refuses NaN, so one NaN focal length would fail every later build.
    #[test]
    fn a_non_finite_lens_resets_to_the_front_view() {
        let mut c = Camera::default_for(None);
        c.focal_length_mm = f64::NAN;
        c.sensor_mm = f64::INFINITY;
        c.focus_distance_mm = f64::NEG_INFINITY;
        c.f_stop = f64::NAN;
        let k = c.clamped(&ViewRange::default());
        assert_eq!(
            (k.focal_length_mm, k.sensor_mm, k.focus_distance_mm, k.f_stop),
            (100.0, 36.0, 60.0, 11.0)
        );
    }
}
