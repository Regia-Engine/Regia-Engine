// SPDX-License-Identifier: MPL-2.0
//! `.decal.ron` version 3 — a projected-decal prefab (`docs/formats/decal-ron.md`).
//!
//! The file is frozen the same way `.vfx.ron` is: `deny_unknown_fields`, a `version` field, and
//! a migration hook. v2 (ADR-3255) adds `metallic`, the `metallic` channel and `shading`
//! (`auto` or `post`) for forward decals; every new field has a default, so the v1 → v2 step only
//! moves the version. v3 adds an optional cooked image path; v1/v2 omit it.
//! [`decal_migration_sources`] lists both migration sources.

use std::collections::HashMap;
use std::path::Path;

use regia_core::math::Vec3;
use regia_ecs::prelude::*;
use serde::{Deserialize, Serialize};

use crate::decal::{
    CHANNEL_ALBEDO, CHANNEL_EMISSIVE, CHANNEL_METALLIC, CHANNEL_NORMAL, CHANNEL_ROUGHNESS, Decal,
    DecalShading, DecalSurface, LAYER_CHARACTER, LAYER_STATIC_MESH, LAYER_TERRAIN, LAYER_WORLD,
    align_to_normal,
};

/// Version of `.decal.ron` this crate reads and writes.
pub const DECAL_FORMAT_VERSION: u32 = 3;

/// Errors from parsing a `.decal.ron` prefab.
#[derive(Debug, thiserror::Error)]
pub enum DecalFormatError {
    /// RON syntax or serde failure.
    #[error("decal parse error: {0}")]
    Parse(String),
    /// `version` is not the one this build reads, and no migrator covers it.
    #[error("decal prefab has version {found}, but this engine reads version {expected}")]
    Version {
        /// Version found in the file.
        found: u32,
        /// Version this build reads ([`DECAL_FORMAT_VERSION`]).
        expected: u32,
    },
    /// A migration step is missing for an older file.
    #[error("decal migration failed: {0}")]
    Migration(String),
    /// The file parsed but failed validation.
    #[error("decal prefab invalid: {0}")]
    Invalid(String),
    /// Directory or file IO failed.
    #[error("decal prefab io: {0}")]
    Io(String),
}

/// One decal prefab on disk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecalPrefab {
    /// Format version. Must be [`DECAL_FORMAT_VERSION`] after migration.
    pub version: u32,
    /// Stable name (`scorch`, `pad_marking`, …).
    pub name: String,
    /// Half extents `(across, along, depth)` in metres.
    pub half_size: [f32; 3],
    /// sRGB hex albedo (`#rrggbb` or `#rrggbbaa`).
    #[serde(default = "default_albedo")]
    pub albedo: String,
    /// Opacity in `0..=1`.
    #[serde(default = "one_f32")]
    pub opacity: f32,
    /// `0` smooth, `1` rough.
    #[serde(default = "one_f32")]
    pub roughness: f32,
    /// Relight strength from the receiver normal.
    #[serde(default)]
    pub normal_strength: f32,
    /// Tangent-space normal xy.
    #[serde(default)]
    pub normal_xy: [f32; 2],
    /// sRGB hex emissive. `"#000000"` adds nothing.
    #[serde(default = "default_emissive")]
    pub emissive: String,
    /// Angle-fade smoothstep on `|dot(normal, projection)|`. `end <= start` disables it.
    #[serde(default = "default_angle")]
    pub angle_fade: [f32; 2],
    /// Distance fade in metres. `end <= start` disables it.
    #[serde(default)]
    pub distance_fade: [f32; 2],
    /// Higher draws later.
    #[serde(default)]
    pub priority: i16,
    /// Receiver layer bits. `3` is terrain | static mesh (not characters).
    #[serde(default = "default_mask")]
    pub receiver_mask: u32,
    /// `solid`, `scorch`, `pad`, `rut` or `grime`.
    #[serde(default = "default_atlas")]
    pub atlas: String,
    /// Seconds until the decal retires. `None` stays until the pool recycles it.
    #[serde(default)]
    pub lifetime: Option<f32>,
    /// `0` hard box, `1` feathered edge.
    #[serde(default = "one_f32")]
    pub edge_softness: f32,
    /// `+`-separated channels: `albedo`, `normal`, `roughness`, `emissive`.
    #[serde(default = "default_channels")]
    pub channels: String,
    /// `modulate` or `replace`.
    #[serde(default = "default_blend")]
    pub blend: String,
    /// v2: metallic written with the `metallic` channel (forward decals only), `0..=1`.
    #[serde(default)]
    pub metallic: f32,
    /// v2: `auto` (forward when the forward-decal switch is on) or `post` (always the projected
    /// post pass).
    #[serde(default = "default_shading")]
    pub shading: String,
    /// v3: cooked RGBA8 image path relative to the prefab directory. Overrides `atlas`.
    #[serde(default)]
    pub texture: Option<String>,
    /// v3 (ADR-8829): the footstep surface of the ground this decal covers (`puddle`, `oil`, …),
    /// answered by [`crate::DecalPool::footstep_surface_at`]. `None` leaves footsteps to the
    /// receiver underneath. 1 to 32 bytes of lowercase ascii, digits and `_`. Presentation only:
    /// navigation and the simulation never read it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surface: Option<String>,
    // A library resolves this before placement. Never serialized or accepted from content.
    #[serde(skip)]
    pub(crate) resolved_texture: Option<(String, i32)>,
}

fn default_shading() -> String {
    "auto".into()
}

fn default_albedo() -> String {
    "#ffffff".into()
}
fn default_emissive() -> String {
    "#000000".into()
}
fn one_f32() -> f32 {
    1.0
}
fn default_angle() -> [f32; 2] {
    [0.25, 0.6]
}
fn default_mask() -> u32 {
    LAYER_WORLD
}
fn default_atlas() -> String {
    "solid".into()
}
fn default_channels() -> String {
    "albedo".into()
}
fn default_blend() -> String {
    "modulate".into()
}

impl DecalPrefab {
    /// Parses a `.decal.ron` document, rejecting unknown versions.
    pub fn from_ron(text: &str) -> Result<Self, DecalFormatError> {
        let text = migrate_decal_ron(text)?;
        let prefab: Self = ron::from_str(&text).map_err(|err| DecalFormatError::Parse(err.to_string()))?;
        prefab.validate()?;
        Ok(prefab)
    }

    /// Writes canonical RON.
    pub fn to_ron(&self) -> Result<String, DecalFormatError> {
        ron::ser::to_string_pretty(self, regia_core::ron_pretty())
            .map_err(|err| DecalFormatError::Parse(err.to_string()))
    }

    fn validate(&self) -> Result<(), DecalFormatError> {
        if self.version != DECAL_FORMAT_VERSION {
            return Err(DecalFormatError::Version {
                found: self.version,
                expected: DECAL_FORMAT_VERSION,
            });
        }
        if self.name.is_empty() {
            return Err(DecalFormatError::Invalid("name is empty".into()));
        }
        if self.half_size.iter().any(|v| *v <= 0.0 || !v.is_finite()) {
            return Err(DecalFormatError::Invalid(
                "half_size must be finite and positive".into(),
            ));
        }
        if !(0.0..=1.0).contains(&self.opacity) {
            return Err(DecalFormatError::Invalid("opacity out of 0..=1".into()));
        }
        // `clamp` keeps NaN, so a NaN in a hand-edited file reached the decal pass as NaN pixels.
        // Regression: tests/tqs_sweep.rs::decal_prefab_refuses_non_finite_numbers.
        let numbers = [
            self.roughness,
            self.normal_strength,
            self.normal_xy[0],
            self.normal_xy[1],
            self.angle_fade[0],
            self.angle_fade[1],
            self.distance_fade[0],
            self.distance_fade[1],
            self.edge_softness,
            self.metallic,
        ];
        if !numbers.iter().all(|v| v.is_finite()) {
            return Err(DecalFormatError::Invalid(
                "roughness, normal, fades and edge softness must be finite".into(),
            ));
        }
        if self.lifetime.is_some_and(|l| !(l.is_finite() && l >= 0.0)) {
            return Err(DecalFormatError::Invalid(
                "lifetime must be finite and >= 0".into(),
            ));
        }
        parse_hex(&self.albedo)?;
        parse_hex(&self.emissive)?;
        atlas_layer(&self.atlas)?;
        if let Some(path) = &self.texture {
            crate::decal_texture::validate_texture_path(path)?;
        }
        if let Some(surface) = &self.surface {
            validate_surface_name(surface)?;
        }
        parse_channels(&self.channels)?;
        parse_blend(&self.blend)?;
        parse_shading(&self.shading)?;
        Ok(())
    }

    /// The runtime box + surface, projected along `normal`.
    pub fn instantiate(
        &self,
        position: Vec3,
        normal: Vec3,
        yaw: f32,
        lifetime: Option<f32>,
    ) -> Result<(Decal, DecalSurface), DecalFormatError> {
        self.validate()?;
        let albedo = parse_hex(&self.albedo)?;
        let emissive = parse_hex(&self.emissive)?;
        let surface = DecalSurface {
            albedo: [albedo[0], albedo[1], albedo[2]],
            opacity: self.opacity,
            roughness: self.roughness.clamp(0.0, 1.0),
            normal_xy: self.normal_xy,
            normal_strength: self.normal_strength.clamp(0.0, 1.0),
            emissive: [emissive[0], emissive[1], emissive[2]],
            angle_fade: self.angle_fade,
            distance_fade: self.distance_fade,
            priority: self.priority,
            receiver_mask: self.receiver_mask,
            atlas_layer: match &self.texture {
                None => atlas_layer(&self.atlas)?,
                Some(path) => self
                    .resolved_texture
                    .as_ref()
                    .filter(|(resolved, _)| resolved == path)
                    .map(|(_, layer)| *layer)
                    .ok_or_else(|| {
                        DecalFormatError::Invalid(format!(
                            "texture '{path}' is unresolved; load it through DecalLibrary"
                        ))
                    })?,
            },
            channels: parse_channels(&self.channels)?,
            edge_softness: self.edge_softness.clamp(0.0, 1.0),
            blend: parse_blend(&self.blend)?,
            cooling_s: 0.0,
            metallic: self.metallic.clamp(0.0, 1.0),
            shading: parse_shading(&self.shading)?,
        };
        let decal = Decal {
            position,
            rotation: align_to_normal(normal, yaw),
            half_size: Vec3::from(self.half_size),
            color: [albedo[0], albedo[1], albedo[2], self.opacity],
            age: 0.0,
            lifetime: lifetime.or(self.lifetime),
        };
        Ok((decal, surface))
    }
}

/// Longest footstep surface name a v3 prefab may carry.
pub const DECAL_SURFACE_NAME_MAX: usize = 32;

/// Checks a v3 footstep `surface` name: 1 to [`DECAL_SURFACE_NAME_MAX`] bytes of lowercase ascii,
/// digits and `_` (the spelling footstep banks use for surface keys).
pub fn validate_surface_name(name: &str) -> Result<(), DecalFormatError> {
    if name.is_empty()
        || name.len() > DECAL_SURFACE_NAME_MAX
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        return Err(DecalFormatError::Invalid(format!(
            "surface must be 1 to {DECAL_SURFACE_NAME_MAX} bytes of lowercase ascii, digits and _"
        )));
    }
    Ok(())
}

fn parse_hex(hex: &str) -> Result<[f32; 4], DecalFormatError> {
    regia_core::math::Color::from_hex(hex)
        .map(|c| c.to_vec4().to_array())
        .map_err(|err| DecalFormatError::Invalid(format!("bad hex color '{hex}': {err}")))
}

/// Atlas layer index. Negative means solid.
pub fn atlas_layer(name: &str) -> Result<i32, DecalFormatError> {
    Ok(match name {
        "solid" => -1,
        "scorch" => 0,
        "pad" => 1,
        "rut" => 2,
        "grime" => 3,
        other => {
            return Err(DecalFormatError::Invalid(format!(
                "unknown atlas '{other}' (solid, scorch, pad, rut, grime)"
            )));
        }
    })
}

fn parse_channels(text: &str) -> Result<u32, DecalFormatError> {
    let mut bits = 0u32;
    for part in text.split('+') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        bits |= match part {
            "albedo" => CHANNEL_ALBEDO,
            "normal" => CHANNEL_NORMAL,
            "roughness" => CHANNEL_ROUGHNESS,
            "emissive" => CHANNEL_EMISSIVE,
            "metallic" => CHANNEL_METALLIC,
            other => {
                return Err(DecalFormatError::Invalid(format!("unknown channel '{other}'")));
            }
        };
    }
    if bits == 0 {
        bits = CHANNEL_ALBEDO;
    }
    Ok(bits)
}

fn parse_shading(text: &str) -> Result<DecalShading, DecalFormatError> {
    match text {
        "auto" => Ok(DecalShading::Auto),
        "post" => Ok(DecalShading::Post),
        other => Err(DecalFormatError::Invalid(format!(
            "unknown shading '{other}' (auto, post)"
        ))),
    }
}

fn parse_blend(text: &str) -> Result<f32, DecalFormatError> {
    match text {
        "modulate" => Ok(0.0),
        "replace" => Ok(1.0),
        other => Err(DecalFormatError::Invalid(format!(
            "unknown blend '{other}' (modulate, replace)"
        ))),
    }
}

/// Rewrites a `.decal.ron` document to [`DECAL_FORMAT_VERSION`].
///
/// v1/v2 → v3 moves the version: `metallic`, `shading` and `texture` have defaults.
/// Older files keep their original material and built-in atlas selection. Comments and formatting are
/// kept. Unknown future versions and files with no `version` field fail.
pub fn migrate_decal_ron(text: &str) -> Result<String, DecalFormatError> {
    let version = regia_core::version::peek_ron_version(text).map_err(DecalFormatError::Migration)?;
    if version == DECAL_FORMAT_VERSION {
        return Ok(text.to_string());
    }
    if version > DECAL_FORMAT_VERSION {
        return Err(DecalFormatError::Version {
            found: version,
            expected: DECAL_FORMAT_VERSION,
        });
    }
    if version == 1 || version == 2 {
        return regia_core::version::rewrite_ron_version(text, "version", DECAL_FORMAT_VERSION)
            .ok_or_else(|| DecalFormatError::Migration("decal has no top-level `version`".into()));
    }
    Err(DecalFormatError::Migration(format!(
        "no migrator from decal v{version} to v{DECAL_FORMAT_VERSION}"
    )))
}

/// Versions migrated directly to v3: v1 and v2.
pub fn decal_migration_sources() -> Vec<u32> {
    vec![1, 2]
}

/// Serde field-list fingerprint of `.decal.ron`, for `cargo xtask check-formats` once the
/// format is registered there.
pub fn decal_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[regia_core::schema::serde_field_names::<DecalPrefab>()])
}

/// Built-in prefabs shipped with the engine (also the `.decal.ron` examples).
pub fn builtin_prefab(name: &str) -> Option<DecalPrefab> {
    let text = match name {
        "scorch" => include_str!("../assets/decal.example.ron"),
        "pad_marking" => PAD_RON,
        "rut" => RUT_RON,
        "grime" => GRIME_RON,
        _ => return None,
    };
    // Strip `//` comment lines so the example file (which documents itself) still parses.
    let stripped = strip_line_comments(text);
    DecalPrefab::from_ron(&stripped).ok()
}

fn strip_line_comments(text: &str) -> String {
    text.lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

const PAD_RON: &str = r##"
(
    version: 2,
    name: "pad_marking",
    half_size: (2.0, 0.55, 0.2),
    albedo: "#f4f1ea",
    opacity: 1.0,
    roughness: 0.55,
    normal_strength: 0.0,
    normal_xy: (0.0, 0.0),
    emissive: "#000000",
    angle_fade: (0.15, 0.45),
    distance_fade: (60.0, 120.0),
    priority: 2,
    receiver_mask: 3,
    atlas: "pad",
    lifetime: None,
    edge_softness: 0.0,
    channels: "albedo",
    blend: "replace",
)
"##;

const RUT_RON: &str = r##"
(
    version: 2,
    name: "rut",
    half_size: (0.22, 0.45, 0.12),
    albedo: "#1c140e",
    opacity: 0.9,
    roughness: 1.0,
    normal_strength: 0.35,
    normal_xy: (0.0, 0.0),
    emissive: "#000000",
    angle_fade: (0.2, 0.5),
    distance_fade: (30.0, 70.0),
    priority: 1,
    receiver_mask: 3,
    atlas: "rut",
    lifetime: None,
    edge_softness: 0.4,
    channels: "albedo+normal",
    blend: "modulate",
)
"##;

const GRIME_RON: &str = r##"
(
    version: 2,
    name: "grime",
    half_size: (1.8, 1.8, 0.25),
    albedo: "#3a342c",
    opacity: 0.55,
    roughness: 0.85,
    normal_strength: 0.2,
    normal_xy: (0.0, 0.0),
    emissive: "#000000",
    angle_fade: (0.1, 0.4),
    distance_fade: (25.0, 50.0),
    priority: -1,
    receiver_mask: 3,
    atlas: "grime",
    lifetime: None,
    edge_softness: 1.0,
    channels: "albedo+roughness",
    blend: "modulate",
)
"##;

/// Named decal prefabs. Starts with the four built-ins; levels can [`DecalLibrary::add`] more.
#[derive(Resource, Debug, Clone)]
pub struct DecalLibrary {
    by_name: HashMap<String, DecalPrefab>,
    texture_layers: HashMap<std::path::PathBuf, u32>,
    texture_atlas: std::sync::Arc<crate::decal_texture::DecalTextureAtlas>,
}

impl Default for DecalLibrary {
    fn default() -> Self {
        Self::builtin()
    }
}

impl DecalLibrary {
    /// The four built-in prefabs.
    pub fn builtin() -> Self {
        let mut lib = Self::new_empty();
        for name in ["scorch", "pad_marking", "rut", "grime"] {
            if let Some(prefab) = builtin_prefab(name) {
                lib.by_name.insert(prefab.name.clone(), prefab);
            }
        }
        lib
    }

    /// An empty library (no prefabs, not even the built-ins).
    pub fn new_empty() -> Self {
        Self {
            by_name: HashMap::new(),
            texture_layers: HashMap::new(),
            texture_atlas: std::sync::Arc::new(crate::decal_texture::DecalTextureAtlas::default()),
        }
    }

    /// Looks a prefab up by name.
    pub fn get(&self, name: &str) -> Option<&DecalPrefab> {
        self.by_name.get(name)
    }

    /// Inserts or replaces a prefab. A custom texture remains unresolved; use
    /// [`Self::add_textured`] or [`Self::load_file`] to resolve it before placement.
    pub fn add(&mut self, mut prefab: DecalPrefab) {
        prefab.resolved_texture = None;
        self.by_name.insert(prefab.name.clone(), prefab);
    }

    /// Loads every `*.decal.ron` in `dir` (non-recursive). Returns how many were added.
    ///
    /// The paths are sorted before loading, so two files that set the same `name` resolve the
    /// same way on every filesystem, and a parse or IO error commits nothing: earlier files are
    /// staged in a temporary map that is merged into the library only when the whole directory
    /// loads (a half-loaded directory must not shadow the built-in prefabs differently per
    /// machine; `VfxLibrary::load_dir` already sorts the same way).
    pub fn load_dir(&mut self, dir: &Path) -> Result<usize, DecalFormatError> {
        let entries = std::fs::read_dir(dir).map_err(|err| DecalFormatError::Io(err.to_string()))?;
        let paths = collect_decal_paths(entries.map(|entry| entry.map(|e| e.path())))?;
        let mut staged = self.clone();
        let n = paths.len();
        for path in paths {
            staged.load_file(&path)?;
        }
        *self = staged;
        Ok(n)
    }

    /// The immutable atlas used by both runtime rendering paths.
    pub fn texture_atlas(&self) -> &std::sync::Arc<crate::decal_texture::DecalTextureAtlas> {
        &self.texture_atlas
    }

    /// Registers a prefab with a host-decoded image (for example from an asset server).
    /// The prefab must name a `texture`; equal names share one stable atlas layer.
    /// An invalid image or a full atlas changes nothing.
    pub fn add_textured(
        &mut self,
        prefab: DecalPrefab,
        image: &regia_asset::CookedImage,
    ) -> Result<(), DecalFormatError> {
        let path = prefab
            .texture
            .as_ref()
            .ok_or_else(|| DecalFormatError::Invalid("textured prefab needs a texture path".into()))?;
        let key = std::path::PathBuf::from(path);
        self.insert_textured(prefab, key, image)
    }

    /// Registers `prefab` with cell `cell` of a cook-side damage atlas (ADR-8829) as its v3
    /// custom image. The prefab's `texture` is set to [`crate::damage_atlas::damage_decal_cell_key`],
    /// so the same cell registered twice shares one atlas layer. A bad cell index, an atlas of the
    /// wrong size, an invalid prefab or a full decal atlas changes nothing.
    pub fn add_damage_cell(
        &mut self,
        mut prefab: DecalPrefab,
        atlas: &[u8],
        cell: u32,
    ) -> Result<(), DecalFormatError> {
        let image = crate::damage_atlas::damage_decal_cell_image(atlas, cell).ok_or_else(|| {
            DecalFormatError::Invalid(format!(
                "damage atlas cell {cell} is not below {} or the atlas is not {}x{} RGBA8",
                crate::damage_atlas::DAMAGE_DECAL_CELLS,
                crate::damage_atlas::DAMAGE_DECAL_ATLAS_SIZE,
                crate::damage_atlas::DAMAGE_DECAL_ATLAS_SIZE
            ))
        })?;
        prefab.texture = Some(crate::damage_atlas::damage_decal_cell_key(cell));
        self.add_textured(prefab, &image)
    }

    fn insert_textured(
        &mut self,
        mut prefab: DecalPrefab,
        key: std::path::PathBuf,
        image: &regia_asset::CookedImage,
    ) -> Result<(), DecalFormatError> {
        prefab.validate()?;
        let layer = self
            .texture_layers
            .get(&key)
            .copied()
            .unwrap_or(self.texture_atlas.layers());
        let atlas = self.texture_atlas.with_image(layer, image)?;
        prefab.resolved_texture = prefab.texture.clone().map(|path| (path, layer as i32));
        self.texture_layers.insert(key, layer);
        self.texture_atlas = std::sync::Arc::new(atlas);
        self.by_name.insert(prefab.name.clone(), prefab);
        Ok(())
    }

    /// Loads one prefab and its optional cooked image atomically. Paths are relative to the
    /// prefab directory and cannot escape it, including through symlinks. Existing layers
    /// retain their indices on reload so live decals keep referencing the same texture.
    pub fn load_file(&mut self, path: &Path) -> Result<(), DecalFormatError> {
        let _span = tracing::debug_span!("load_decal_prefab").entered();
        let io = |e: std::io::Error| DecalFormatError::Io(e.to_string());
        let text = std::fs::read_to_string(path).map_err(io)?;
        let prefab = DecalPrefab::from_ron(&text)?;
        self.load_prefab(prefab, path.parent().unwrap_or_else(|| Path::new(".")))
    }

    pub(crate) fn load_prefab(&mut self, prefab: DecalPrefab, parent: &Path) -> Result<(), DecalFormatError> {
        let io = |e: std::io::Error| DecalFormatError::Io(e.to_string());
        let Some(texture) = &prefab.texture else {
            self.add(prefab);
            return Ok(());
        };
        let parent = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        let root = std::fs::canonicalize(parent).map_err(io)?;
        let image_path = std::fs::canonicalize(root.join(texture)).map_err(io)?;
        if !image_path.starts_with(&root) {
            return Err(DecalFormatError::Invalid(
                "texture escapes the prefab directory".into(),
            ));
        }
        // Read a bounded amount even if the file grows after metadata was checked.
        use std::io::Read;
        const MAX_BYTES: u64 = 64 * 1024 * 1024;
        let file = std::fs::File::open(&image_path).map_err(io)?;
        let mut bytes = Vec::new();
        file.take(MAX_BYTES + 1).read_to_end(&mut bytes).map_err(io)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(DecalFormatError::Invalid("decal image exceeds 64 MiB".into()));
        }
        let (_, image) = regia_asset::load_image_bytes(&bytes)
            .map_err(|e| DecalFormatError::Invalid(format!("texture '{texture}': {e}")))?;
        self.insert_textured(prefab, image_path, &image)
    }

    /// Names in sorted order.
    pub fn names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.by_name.keys().map(String::as_str).collect();
        names.sort_unstable();
        names
    }
}

/// A level-placed decal. Identity rotation projects along world +Y (flat on the ground);
/// any other rotation is the decal basis (local +Z projects). Scale multiplies `half_size`.
#[derive(Component, Reflect, Clone, Debug, Default)]
#[reflect(Component, Default)]
pub struct StaticDecal {
    /// Prefab name in the [`DecalLibrary`].
    pub prefab: String,
    /// Extra yaw in radians around the projection axis.
    pub yaw: f32,
    /// When non-zero, replaces the prefab receiver mask.
    pub receiver_mask: u32,
}

/// Collects the `*.decal.ron` paths of a directory scan, sorted. An unreadable directory entry
/// fails the whole scan (review fix-vfx2-determinism finding 3: silently dropping an entry
/// would stage a truncated prefab set whose contents depend on the filesystem's state, the
/// divergence the staged commit exists to prevent).
fn collect_decal_paths(
    entries: impl IntoIterator<Item = std::io::Result<std::path::PathBuf>>,
) -> Result<Vec<std::path::PathBuf>, DecalFormatError> {
    let mut paths: Vec<std::path::PathBuf> = entries
        .into_iter()
        .map(|entry| entry.map_err(|err| DecalFormatError::Io(err.to_string())))
        .collect::<Result<_, _>>()?;
    paths.retain(|path| {
        path.file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.ends_with(".decal.ron"))
    });
    paths.sort();
    Ok(paths)
}

/// Confirms the built-in masks never include characters. Used by tests and the agent result.
pub fn world_mask_excludes_characters(mask: u32) -> bool {
    mask & LAYER_CHARACTER == 0 && mask & LAYER_TERRAIN != 0 && mask & LAYER_STATIC_MESH != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The crate's compiled-in copy of the documented example is byte-identical to the one under
    /// `docs/formats/` (ADR-1485). The check only runs inside a repository checkout: a packaged
    /// or vendored copy has no `docs/`.
    #[test]
    fn builtin_scorch_matches_documented_example() {
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        if !manifest.join("../../Cargo.toml").is_file() {
            return;
        }
        let documented = std::fs::read_to_string(manifest.join("../../docs/formats/decal.example.ron"))
            .expect("docs/formats/decal.example.ron");
        assert_eq!(
            include_str!("../assets/decal.example.ron"),
            documented,
            "copy crates/regia_vfx/assets/decal.example.ron over docs/formats/decal.example.ron (or the reverse)"
        );
    }

    #[test]
    fn scorch_example_round_trips_and_rejects_characters() {
        let prefab = builtin_prefab("scorch").expect("scorch parses");
        assert_eq!(prefab.version, 3);
        assert!(world_mask_excludes_characters(prefab.receiver_mask));
        let ron = prefab.to_ron().expect("serialize");
        let again = DecalPrefab::from_ron(&ron).expect("reparse");
        assert_eq!(again.name, "scorch");
        let (decal, surface) = prefab
            .instantiate(Vec3::new(1.0, 0.0, 2.0), Vec3::Y, 0.0, None)
            .expect("place");
        assert!(((decal.rotation * Vec3::Z) - Vec3::Y).length() < 1e-4);
        assert_eq!(surface.atlas_layer, 0);
        assert!(surface.channels & CHANNEL_NORMAL != 0);
        assert!(surface.channels & CHANNEL_ROUGHNESS != 0);
        assert!((surface.blend - 0.0).abs() < 1e-6);
    }

    #[test]
    fn pad_is_crisp_replace_and_rut_is_a_strip_stamp() {
        let pad = builtin_prefab("pad_marking").expect("pad");
        let (_, surface) = pad.instantiate(Vec3::ZERO, Vec3::Y, 0.0, None).expect("place");
        assert_eq!(surface.atlas_layer, 1);
        assert!((surface.edge_softness).abs() < 1e-6, "pad edges stay hard");
        assert!((surface.blend - 1.0).abs() < 1e-6);
        let rut = builtin_prefab("rut").expect("rut");
        assert!(
            rut.half_size[0] < rut.half_size[1],
            "a rut stamp is longer than it is wide"
        );
        assert_eq!(DecalLibrary::builtin().names().len(), 4);
    }

    #[test]
    fn load_dir_order_does_not_change_the_library() {
        // Two files set the same name with different albedo; the sorted last one wins on every
        // filesystem (read_dir order is filesystem-dependent).
        let dir = regia_test_utils::unique_temp_dir("vfx-decal-load-order");
        let mut prefab_a = builtin_prefab("scorch").expect("scorch");
        prefab_a.albedo = "#ff0000".into();
        let mut prefab_b = prefab_a.clone();
        prefab_b.albedo = "#00ff00".into();
        std::fs::write(dir.join("a.decal.ron"), prefab_a.to_ron().expect("ron a")).expect("write a");
        std::fs::write(dir.join("z.decal.ron"), prefab_b.to_ron().expect("ron b")).expect("write z");

        let mut lib = DecalLibrary::new_empty();
        let n = lib.load_dir(&dir).expect("load");
        assert_eq!(n, 2, "a.decal.ron also shadows the built-in scorch");
        assert_eq!(
            lib.get("scorch").map(|p| p.albedo.as_str()),
            Some("#00ff00"),
            "the sorted-last file wins, whatever order read_dir yields"
        );

        // An invalid file fails the whole load and commits nothing, even if it sorts first.
        let bad_dir = regia_test_utils::unique_temp_dir("vfx-decal-load-error");
        std::fs::write(bad_dir.join("a.decal.ron"), "(version: 1, name: \"x\"").expect("write invalid");
        std::fs::write(bad_dir.join("z.decal.ron"), prefab_a.to_ron().expect("ron")).expect("write valid");
        let mut lib = DecalLibrary::new_empty();
        assert!(lib.load_dir(&bad_dir).is_err(), "the parse error fails the load");
        assert!(lib.get("scorch").is_none(), "no partial prefix is committed");
        assert_eq!(lib.names(), DecalLibrary::new_empty().names());

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&bad_dir);
    }

    /// Review fix-vfx2-determinism finding 3: a directory-entry IO error (an entry unlinked or
    /// unreadable mid-scan) must fail the load, not silently stage a truncated prefab set —
    /// which prefabs load must not depend on the machine's filesystem state.
    #[test]
    fn load_dir_entry_errors_fail_the_scan() {
        let entries = vec![
            Ok(std::path::PathBuf::from("a.decal.ron")),
            Err(std::io::Error::other("entry gone")),
            Ok(std::path::PathBuf::from("b.decal.ron")),
        ];
        let got = collect_decal_paths(entries);
        assert!(
            matches!(got, Err(DecalFormatError::Io(_))),
            "an entry error fails the scan: {got:?}"
        );
        // Non-decal entries are filtered, the rest sorted.
        let entries = vec![
            Ok(std::path::PathBuf::from("z.decal.ron")),
            Ok(std::path::PathBuf::from("notes.txt")),
            Ok(std::path::PathBuf::from("a.decal.ron")),
        ];
        let got = collect_decal_paths(entries).expect("clean scan");
        assert_eq!(
            got,
            vec![
                std::path::PathBuf::from("a.decal.ron"),
                std::path::PathBuf::from("z.decal.ron")
            ]
        );
    }

    #[test]
    fn unknown_version_and_unknown_field_fail() {
        let future = "(version: 4, name: \"x\", half_size: (1.0, 1.0, 1.0))";
        let err = DecalPrefab::from_ron(future).expect_err("future");
        assert!(err.to_string().contains("version 4"), "{err}");
        let old = "(version: 0, name: \"x\", half_size: (1.0, 1.0, 1.0))";
        let err = DecalPrefab::from_ron(old).expect_err("no migrator");
        assert!(err.to_string().contains("migrat"), "{err}");
        let extra = "(version: 2, name: \"x\", half_size: (1.0, 1.0, 1.0), nope: 1)";
        assert!(DecalPrefab::from_ron(extra).is_err());
        // v3 (ADR-8121) migrates both prior versions.
        assert_eq!(decal_migration_sources(), vec![1, 2]);
        assert!(decal_schema_fingerprint().contains("version"));
    }

    #[test]
    fn a_v1_prefab_migrates_to_v3_with_the_old_meaning() {
        let v1 =
            "// kept\n(version: 1, name: \"x\", half_size: (1.0, 1.0, 1.0), channels: \"albedo+roughness\")";
        let migrated = migrate_decal_ron(v1).expect("v1 migrates");
        assert!(migrated.starts_with("// kept"), "comments survive: {migrated}");
        let prefab = DecalPrefab::from_ron(v1).expect("v1 loads");
        assert_eq!(prefab.version, 3);
        assert_eq!(prefab.shading, "auto");
        assert_eq!(prefab.metallic, 0.0);
        let (_, surface) = prefab.instantiate(Vec3::ZERO, Vec3::Y, 0.0, None).expect("place");
        assert_eq!(surface.shading, DecalShading::Auto);
        assert_eq!(surface.channels, CHANNEL_ALBEDO | CHANNEL_ROUGHNESS);
        assert_eq!(decal_migration_sources(), vec![1, 2]);
    }

    #[test]
    fn v2_reads_metallic_post_shading_and_refuses_bad_values() {
        let text = "(version: 2, name: \"m\", half_size: (1.0, 1.0, 1.0), metallic: 0.7, shading: \"post\", channels: \"albedo+metallic\")";
        let prefab = DecalPrefab::from_ron(text).expect("v2 loads");
        let (_, surface) = prefab.instantiate(Vec3::ZERO, Vec3::Y, 0.0, None).expect("place");
        assert_eq!(surface.shading, DecalShading::Post);
        assert!((surface.metallic - 0.7).abs() < 1e-6);
        assert!(surface.channels & CHANNEL_METALLIC != 0);
        let bad = "(version: 2, name: \"m\", half_size: (1.0, 1.0, 1.0), shading: \"deferred\")";
        assert!(DecalPrefab::from_ron(bad).is_err());
        let nan = "(version: 2, name: \"m\", half_size: (1.0, 1.0, 1.0), metallic: NaN)";
        assert!(DecalPrefab::from_ron(nan).is_err());
    }
}
