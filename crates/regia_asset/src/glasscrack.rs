// SPDX-License-Identifier: MPL-2.0
//! `.glasscrack.ron` crack sets (`docs/formats/glass-crack.md`) and their cooked form.
//!
//! A crack set is the authored look of damaged glass for one [`GlassType`]: per damage level
//! (D1–D3) a list of stamp textures plus the shading and escalation rules. This module owns the
//! manifest (parse, validate, migrate, schema fingerprint) and the cooked binary
//! ([`CookedCrackSet`]: the manifest plus decoded stamp pixels). The cooker
//! (`regia_asset_pipeline::glasscrack_cook`) decodes the PNGs; the renderer stamps the cooked
//! pixels into per-instance crack masks (`regia_render::glass`).

use serde::{Deserialize, Serialize};

/// Current `.glasscrack.ron` version.
pub const GLASSCRACK_FORMAT_VERSION: u32 = 1;

/// Magic of a cooked crack set (`FGCS`).
pub const COOKED_CRACK_SET_MAGIC: [u8; 4] = *b"FGCS";

/// Version of the cooked crack-set binary.
pub const COOKED_CRACK_SET_VERSION: u32 = 1;

/// Which hole and shard behaviour a crack set has (DO27 `spec/glass.md` "Glass types").
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GlassType {
    /// Bulletproof polymer laminate (unit canopies): cracks and crazes, stays in the frame; only
    /// small punched-through holes at the worst D3 hits.
    #[default]
    Laminate,
    /// Pressure-rated thick multi-pane glazing (buildings): the outer pane cracks while the inner
    /// pane holds; at D3 the panes fail into chunky fragments.
    PressurePane,
}

impl GlassType {
    /// Wire name (`laminate` / `pressure_pane`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Laminate => "laminate",
            Self::PressurePane => "pressure_pane",
        }
    }

    /// Parses a wire name.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "laminate" => Some(Self::Laminate),
            "pressure_pane" => Some(Self::PressurePane),
            _ => None,
        }
    }
}

/// Where the crack mask page lies on the glass.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DamageSpace {
    /// Planar projection in mesh-local space along the bounding box's thinnest axis.
    #[default]
    Planar,
    /// The mesh's first UV set (must be non-overlapping).
    Uv0,
    /// The mesh's second UV set (must be non-overlapping).
    Uv1,
}

/// One authored crack stamp.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CrackStamp {
    /// Unique within the set.
    pub name: String,
    /// RGBA mask PNG (R cracks, G crazing, B holes, A normal-bend), relative to the manifest.
    pub mask: String,
    /// Tangent-space crack normal PNG, relative to the manifest.
    #[serde(default)]
    pub normal: Option<String>,
    /// Footprint of the whole texture on the glass, metres (width, height).
    pub size_m: (f32, f32),
    /// Impact point inside the texture, 0..1 from the top-left.
    #[serde(default = "default_pivot")]
    pub pivot: (f32, f32),
    /// Pick weight among the level's variants.
    #[serde(default = "one")]
    pub weight: f32,
    /// Spin the stamp around its pivot per impact.
    #[serde(default = "yes")]
    pub random_rotation: bool,
}

/// How the mask channels shade the glass.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CrackShading {
    /// Frosted scatter colour along cracks (sRGB hex).
    #[serde(with = "hex_color")]
    pub crack_scatter: [f32; 3],
    /// Transmission factor where R = 1.
    pub crack_transmission: f32,
    /// Roughness where R = 1.
    pub crack_roughness: f32,
    /// Milky haze opacity where G = 1.
    pub crazing_haze: f32,
    /// Roughness where G = 1.
    pub crazing_roughness: f32,
    /// Crack-normal strength for lighting and refraction.
    pub normal_bend: f32,
    /// Alpha-test threshold on B.
    pub hole_cutoff: f32,
    /// Jaggedness of hole edges (0 = clean cut).
    pub edge_jitter: f32,
}

impl Default for CrackShading {
    fn default() -> Self {
        Self {
            crack_scatter: hex_color::parse("#e6edf2").unwrap_or([0.8, 0.85, 0.88]),
            crack_transmission: 0.1,
            crack_roughness: 0.7,
            crazing_haze: 0.55,
            crazing_roughness: 0.45,
            normal_bend: 1.0,
            hole_cutoff: 0.5,
            edge_jitter: 0.35,
        }
    }
}

/// Cumulative impact energy at which an instance reaches each level (0 = never automatically).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Escalation {
    /// Energy to reach D1.
    pub d1: f32,
    /// Energy to reach D2.
    pub d2: f32,
    /// Energy to reach D3.
    pub d3: f32,
}

impl Default for Escalation {
    fn default() -> Self {
        Self {
            d1: 1.0,
            d2: 4.0,
            d3: 10.0,
        }
    }
}

impl Escalation {
    /// The level (0–3) a cumulative `energy` reaches.
    #[must_use]
    pub fn level_for(&self, energy: f32) -> u8 {
        let mut level = 0;
        for (index, threshold) in [self.d1, self.d2, self.d3].into_iter().enumerate() {
            if threshold > 0.0 && energy >= threshold {
                level = index as u8 + 1;
            }
        }
        level
    }
}

/// Rules of a laminate set.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LaminateRules {
    /// Holes never exceed this radius around an impact, metres.
    pub max_hole_radius_m: f32,
    /// Only D3 impacts at least this energetic punch a hole.
    pub min_hole_energy: f32,
    /// Milky delamination halo radius around D2/D3 impacts, metres.
    pub halo_radius_m: f32,
}

impl Default for LaminateRules {
    fn default() -> Self {
        Self {
            max_hole_radius_m: 0.035,
            min_hole_energy: 3.0,
            halo_radius_m: 0.08,
        }
    }
}

/// D3 fragment burst of a pressure pane.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ChunkBurst {
    /// `regia_vfx` preset.
    pub preset: String,
    /// Fragment count range (inclusive).
    pub count: (u32, u32),
    /// Fragment edge length range, metres.
    pub size_m: (f32, f32),
}

impl Default for ChunkBurst {
    fn default() -> Self {
        Self {
            preset: "glass_chunks".to_string(),
            count: (8, 16),
            size_m: (0.06, 0.25),
        }
    }
}

/// Rules of a pressure-pane set.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PressurePaneRules {
    /// Outer-to-inner pane distance for the parallax inner pane, metres.
    pub pane_gap_m: f32,
    /// Inner pane transmission tint (sRGB hex).
    #[serde(with = "hex_color")]
    pub inner_tint: [f32; 3],
    /// D3 fragment burst.
    pub chunks: ChunkBurst,
    /// D3 emits a decompression breach event.
    pub breach: bool,
}

impl Default for PressurePaneRules {
    fn default() -> Self {
        Self {
            pane_gap_m: 0.012,
            inner_tint: hex_color::parse("#f2f6f8").unwrap_or([0.9, 0.93, 0.94]),
            chunks: ChunkBurst::default(),
            breach: true,
        }
    }
}

/// A `.glasscrack.ron` manifest.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GlassCrackSet {
    /// Must be [`GLASSCRACK_FORMAT_VERSION`].
    pub format_version: u32,
    /// Stable id.
    pub name: String,
    /// Hole/shard behaviour.
    pub glass_type: GlassType,
    /// Runtime page side (256, 512 or 1024).
    #[serde(default = "default_page")]
    pub page_size: u32,
    /// Where the page lies on the glass.
    #[serde(default)]
    pub damage_space: DamageSpace,
    /// Channel shading.
    #[serde(default)]
    pub shading: CrackShading,
    /// Automatic level thresholds.
    #[serde(default)]
    pub escalation: Escalation,
    /// D1 stamps.
    pub d1: Vec<CrackStamp>,
    /// D2 stamps.
    pub d2: Vec<CrackStamp>,
    /// D3 stamps.
    pub d3: Vec<CrackStamp>,
    /// Laminate rules (required iff `glass_type` is laminate).
    #[serde(default)]
    pub laminate: Option<LaminateRules>,
    /// Pressure-pane rules (required iff `glass_type` is pressure_pane).
    #[serde(default)]
    pub pressure_pane: Option<PressurePaneRules>,
}

/// `v > 0` and finite (not NaN, not infinite).
fn positive(v: f32) -> bool {
    v.is_finite() && v > 0.0
}

fn default_pivot() -> (f32, f32) {
    (0.5, 0.5)
}
fn one() -> f32 {
    1.0
}
fn yes() -> bool {
    true
}
fn default_page() -> u32 {
    512
}

/// Why a crack set failed to load or validate.
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum GlassCrackError {
    /// RON syntax or shape error.
    #[error("glasscrack parse: {0}")]
    Parse(String),
    /// Newer than this engine, or no migrator.
    #[error("glasscrack version {found} (this engine reads {expected})")]
    Version {
        /// Found version.
        found: u32,
        /// Supported version.
        expected: u32,
    },
    /// A validation rule failed.
    #[error("glasscrack `{set}`: {reason}")]
    Invalid {
        /// Set name.
        set: String,
        /// Which rule.
        reason: String,
    },
    /// Cooked binary is truncated or has the wrong magic/version.
    #[error("cooked crack set: {0}")]
    Cooked(String),
}

impl GlassCrackSet {
    /// Parses and validates a manifest (older versions migrate; none exist at v1).
    pub fn from_ron(text: &str) -> Result<Self, GlassCrackError> {
        let text = migrate_glasscrack(text)?;
        let set: Self = ron::from_str(&text).map_err(|e| GlassCrackError::Parse(e.to_string()))?;
        set.validate()?;
        Ok(set)
    }

    /// Pretty RON.
    pub fn to_ron(&self) -> Result<String, GlassCrackError> {
        ron::ser::to_string_pretty(self, regia_core::ron_pretty())
            .map_err(|e| GlassCrackError::Parse(e.to_string()))
    }

    /// Stamps of level 1–3 (empty for 0 or > 3).
    #[must_use]
    pub fn stamps(&self, level: u8) -> &[CrackStamp] {
        match level {
            1 => &self.d1,
            2 => &self.d2,
            3 => &self.d3,
            _ => &[],
        }
    }

    /// Manifest-level rules (texture rules are checked by the cooker,
    /// [`CookedCrackSet::validate_pixels`]).
    pub fn validate(&self) -> Result<(), GlassCrackError> {
        let fail = |reason: String| GlassCrackError::Invalid {
            set: self.name.clone(),
            reason,
        };
        if self.format_version != GLASSCRACK_FORMAT_VERSION {
            return Err(GlassCrackError::Version {
                found: self.format_version,
                expected: GLASSCRACK_FORMAT_VERSION,
            });
        }
        if self.name.is_empty() {
            return Err(fail("name is empty".into()));
        }
        if ![256, 512, 1024].contains(&self.page_size) {
            return Err(fail(format!("page_size {} not 256/512/1024", self.page_size)));
        }
        let mut names = std::collections::HashSet::new();
        for (level, stamps) in [(1, &self.d1), (2, &self.d2), (3, &self.d3)] {
            if stamps.is_empty() {
                return Err(fail(format!("d{level} has no stamps")));
            }
            for stamp in stamps.iter() {
                if !names.insert(stamp.name.as_str()) {
                    return Err(fail(format!("stamp name `{}` repeats", stamp.name)));
                }
                let (w, h) = stamp.size_m;
                if !(positive(w) && positive(h)) {
                    return Err(fail(format!("stamp `{}` size_m must be > 0", stamp.name)));
                }
                let (px, py) = stamp.pivot;
                if !((0.0..=1.0).contains(&px) && (0.0..=1.0).contains(&py)) {
                    return Err(fail(format!("stamp `{}` pivot outside 0..1", stamp.name)));
                }
                if !positive(stamp.weight) {
                    return Err(fail(format!("stamp `{}` weight must be > 0", stamp.name)));
                }
            }
        }
        let s = &self.shading;
        for (name, value, max) in [
            ("crack_transmission", s.crack_transmission, 1.0),
            ("crack_roughness", s.crack_roughness, 1.0),
            ("crazing_haze", s.crazing_haze, 1.0),
            ("crazing_roughness", s.crazing_roughness, 1.0),
            ("hole_cutoff", s.hole_cutoff, 1.0),
            ("edge_jitter", s.edge_jitter, 1.0),
            ("normal_bend", s.normal_bend, 4.0),
        ] {
            if !(0.0..=max).contains(&value) {
                return Err(fail(format!("shading.{name} {value} outside 0..={max}")));
            }
        }
        let mut last = 0.0f32;
        for threshold in [self.escalation.d1, self.escalation.d2, self.escalation.d3] {
            if !(threshold.is_finite() && threshold >= 0.0) {
                return Err(fail("escalation thresholds must be finite and >= 0".into()));
            }
            if threshold > 0.0 {
                if threshold <= last {
                    return Err(fail("escalation thresholds must increase".into()));
                }
                last = threshold;
            }
        }
        match self.glass_type {
            GlassType::Laminate => {
                let Some(rules) = self.laminate else {
                    return Err(fail("glass_type laminate needs `laminate` rules".into()));
                };
                if self.pressure_pane.is_some() {
                    return Err(fail("a laminate set must not have `pressure_pane` rules".into()));
                }
                if !positive(rules.max_hole_radius_m)
                    || !(rules.halo_radius_m.is_finite() && rules.halo_radius_m >= 0.0)
                    || !(rules.min_hole_energy.is_finite() && rules.min_hole_energy >= 0.0)
                {
                    return Err(fail("laminate rules out of range".into()));
                }
            }
            GlassType::PressurePane => {
                let Some(rules) = &self.pressure_pane else {
                    return Err(fail(
                        "glass_type pressure_pane needs `pressure_pane` rules".into(),
                    ));
                };
                if self.laminate.is_some() {
                    return Err(fail("a pressure-pane set must not have `laminate` rules".into()));
                }
                if !positive(rules.pane_gap_m) {
                    return Err(fail("pane_gap_m must be > 0".into()));
                }
                let c = &rules.chunks;
                if c.count.0 < 1 || c.count.1 < c.count.0 {
                    return Err(fail("chunks.count must be 1 <= min <= max".into()));
                }
                if c.size_m.0 < 0.03 || c.size_m.1 < c.size_m.0 {
                    return Err(fail(
                        "chunks.size_m must be 0.03 <= min <= max (chunky, not glitter)".into(),
                    ));
                }
            }
        }
        Ok(())
    }
}

/// Brings an older `.glasscrack.ron` text to the current version (none exist at v1); rejects a
/// newer one.
pub fn migrate_glasscrack(text: &str) -> Result<String, GlassCrackError> {
    let version = regia_core::version::peek_ron_version(text).map_err(GlassCrackError::Parse)?;
    if version == GLASSCRACK_FORMAT_VERSION {
        return Ok(text.to_string());
    }
    Err(GlassCrackError::Version {
        found: version,
        expected: GLASSCRACK_FORMAT_VERSION,
    })
}

/// Versions [`migrate_glasscrack`] accepts below the current one (empty at v1).
#[must_use]
pub fn glasscrack_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// Serde field-list fingerprint of `.glasscrack.ron` (for `cargo xtask check-formats`).
#[must_use]
pub fn glasscrack_schema_fingerprint() -> String {
    use regia_core::schema::{serde_field_names, serde_variant_names};
    regia_core::schema::join_field_lists(&[
        serde_field_names::<GlassCrackSet>(),
        serde_field_names::<CrackStamp>(),
        serde_field_names::<CrackShading>(),
        serde_field_names::<Escalation>(),
        serde_field_names::<LaminateRules>(),
        serde_field_names::<PressurePaneRules>(),
        serde_field_names::<ChunkBurst>(),
        serde_variant_names::<GlassType>(),
        serde_variant_names::<DamageSpace>(),
    ])
}

/// Decoded pixels of one stamp: `size`² RGBA8 mask and RG8 tangent-space normal (X, Y).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StampPixels {
    /// Side length in texels (square).
    pub size: u32,
    /// `size * size * 4` bytes, linear.
    pub mask: Vec<u8>,
    /// `size * size * 2` bytes: normal X, Y in 0..255 (128 = flat).
    pub normal: Vec<u8>,
}

impl StampPixels {
    /// Largest distance from the stamp's pivot to a texel with B ≥ `cutoff`, in metres (texel
    /// offsets are scaled by `size_m`; compare it with `max_hole_radius_m`, not a fraction). A
    /// `cutoff` below 1 behaves as 1, so blue 0 never counts as a hole. 0 when the stamp has no holes.
    #[must_use]
    pub fn hole_extent(&self, pivot: (f32, f32), size_m: (f32, f32), cutoff: u8) -> f32 {
        let n = self.size as usize;
        let mut worst = 0.0f32;
        for y in 0..n {
            for x in 0..n {
                if self.mask[(y * n + x) * 4 + 2] >= cutoff.max(1) {
                    let dx = ((x as f32 + 0.5) / n as f32 - pivot.0) * size_m.0;
                    let dy = ((y as f32 + 0.5) / n as f32 - pivot.1) * size_m.1;
                    worst = worst.max((dx * dx + dy * dy).sqrt());
                }
            }
        }
        worst
    }
}

/// A cooked crack set: the manifest plus decoded stamps in manifest order (D1, then D2, D3).
#[derive(Clone, Debug, PartialEq)]
pub struct CookedCrackSet {
    /// The validated manifest.
    pub manifest: GlassCrackSet,
    /// Stamp pixels for `manifest.d1 ++ d2 ++ d3`.
    pub stamps: Vec<StampPixels>,
}

impl CookedCrackSet {
    /// Index range of a level's stamps in [`Self::stamps`].
    #[must_use]
    pub fn level_range(&self, level: u8) -> std::ops::Range<usize> {
        let (a, b) = (self.manifest.d1.len(), self.manifest.d2.len());
        match level {
            1 => 0..a,
            2 => a..a + b,
            3 => a + b..a + b + self.manifest.d3.len(),
            _ => 0..0,
        }
    }

    /// Texture rules: sizes, no holes before D3, laminate hole radius.
    pub fn validate_pixels(&self) -> Result<(), GlassCrackError> {
        let m = &self.manifest;
        let fail = |reason: String| GlassCrackError::Invalid {
            set: m.name.clone(),
            reason,
        };
        let total = m.d1.len() + m.d2.len() + m.d3.len();
        if self.stamps.len() != total {
            return Err(fail(format!(
                "{} stamps cooked for {total} declared",
                self.stamps.len()
            )));
        }
        let hole_cut = (m.shading.hole_cutoff * 255.0).round() as u8;
        for level in 1..=3u8 {
            for (decl, px) in m.stamps(level).iter().zip(&self.stamps[self.level_range(level)]) {
                let n = px.size;
                if !(128..=1024).contains(&n) || !n.is_power_of_two() {
                    return Err(fail(format!(
                        "stamp `{}` is {n}², not a power of two in 128..=1024",
                        decl.name
                    )));
                }
                let texels = (n * n) as usize;
                if px.mask.len() != texels * 4 || px.normal.len() != texels * 2 {
                    return Err(fail(format!(
                        "stamp `{}` pixel buffers have the wrong length",
                        decl.name
                    )));
                }
                let has_hole = px.mask.as_chunks::<4>().0.iter().any(|t| t[2] > 0);
                if level < 3 && has_hole {
                    return Err(fail(format!(
                        "D{level} stamp `{}` has holes (B > 0); holes are D3 only",
                        decl.name
                    )));
                }
                if level == 3
                    && let Some(rules) = m.laminate
                {
                    let extent = px.hole_extent(decl.pivot, decl.size_m, hole_cut);
                    if extent > rules.max_hole_radius_m {
                        return Err(fail(format!(
                            "laminate D3 stamp `{}` has a hole {extent:.3} m from its pivot (max {:.3} m)",
                            decl.name, rules.max_hole_radius_m
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    /// Cooked bytes: magic, version, manifest RON length + text, then per stamp
    /// `size: u32` + mask + normal (little endian).
    pub fn to_bytes(&self) -> Result<Vec<u8>, GlassCrackError> {
        let ron = self.manifest.to_ron()?;
        let mut out = Vec::with_capacity(
            16 + ron.len()
                + self
                    .stamps
                    .iter()
                    .map(|s| s.mask.len() * 3 / 2 + 4)
                    .sum::<usize>(),
        );
        out.extend_from_slice(&COOKED_CRACK_SET_MAGIC);
        out.extend_from_slice(&COOKED_CRACK_SET_VERSION.to_le_bytes());
        out.extend_from_slice(&(ron.len() as u32).to_le_bytes());
        out.extend_from_slice(ron.as_bytes());
        out.extend_from_slice(&(self.stamps.len() as u32).to_le_bytes());
        for stamp in &self.stamps {
            out.extend_from_slice(&stamp.size.to_le_bytes());
            out.extend_from_slice(&stamp.mask);
            out.extend_from_slice(&stamp.normal);
        }
        Ok(out)
    }

    /// Reads [`Self::to_bytes`] output and validates it.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, GlassCrackError> {
        let bad = |what: &str| GlassCrackError::Cooked(what.to_string());
        let mut at = 0usize;
        let mut take = |n: usize| -> Result<&[u8], GlassCrackError> {
            let slice = bytes.get(at..at + n).ok_or_else(|| bad("truncated"))?;
            at += n;
            Ok(slice)
        };
        if take(4)? != COOKED_CRACK_SET_MAGIC {
            return Err(bad("bad magic"));
        }
        let u32_of = |s: &[u8]| u32::from_le_bytes([s[0], s[1], s[2], s[3]]);
        let version = u32_of(take(4)?);
        if version != COOKED_CRACK_SET_VERSION {
            return Err(GlassCrackError::Version {
                found: version,
                expected: COOKED_CRACK_SET_VERSION,
            });
        }
        let ron_len = u32_of(take(4)?) as usize;
        let text = std::str::from_utf8(take(ron_len)?).map_err(|_| bad("manifest is not UTF-8"))?;
        let manifest = GlassCrackSet::from_ron(text)?;
        let count = u32_of(take(4)?) as usize;
        let mut stamps = Vec::with_capacity(count.min(64));
        for _ in 0..count {
            let size = u32_of(take(4)?);
            if size > 4096 {
                return Err(bad("stamp size too large"));
            }
            let texels = (size * size) as usize;
            let mask = take(texels * 4)?.to_vec();
            let normal = take(texels * 2)?.to_vec();
            stamps.push(StampPixels { size, mask, normal });
        }
        let cooked = Self { manifest, stamps };
        cooked.validate_pixels()?;
        Ok(cooked)
    }
}

/// `#rrggbb` ↔ linear-free sRGB triple in 0..1 (the shader decodes sRGB).
mod hex_color {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn parse(text: &str) -> Option<[f32; 3]> {
        let hex = text.strip_prefix('#')?;
        if hex.len() != 6 {
            return None;
        }
        let byte = |i: usize| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok();
        Some([
            f32::from(byte(0)?) / 255.0,
            f32::from(byte(2)?) / 255.0,
            f32::from(byte(4)?) / 255.0,
        ])
    }

    pub fn serialize<S: Serializer>(value: &[f32; 3], s: S) -> Result<S::Ok, S::Error> {
        let b = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
        s.serialize_str(&format!(
            "#{:02x}{:02x}{:02x}",
            b(value[0]),
            b(value[1]),
            b(value[2])
        ))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[f32; 3], D::Error> {
        let text = String::deserialize(d)?;
        parse(&text).ok_or_else(|| serde::de::Error::custom(format!("`{text}` is not #rrggbb")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LAMINATE: &str = include_str!("../../../docs/formats/glasscrack.example.ron");
    const PRESSURE: &str = include_str!("../../../docs/formats/glasscrack-pressure.example.ron");

    #[test]
    fn both_examples_parse_and_round_trip() {
        for text in [LAMINATE, PRESSURE] {
            let set = GlassCrackSet::from_ron(text).expect("example parses");
            let again = GlassCrackSet::from_ron(&set.to_ron().expect("ron")).expect("round trip");
            assert_eq!(set, again);
        }
        let lam = GlassCrackSet::from_ron(LAMINATE).expect("laminate");
        assert_eq!(lam.glass_type, GlassType::Laminate);
        assert_eq!(lam.d1[2].pivot, (0.5, 0.5));
        assert!((lam.d1[2].weight - 0.5).abs() < 1e-6);
        let pp = GlassCrackSet::from_ron(PRESSURE).expect("pressure");
        assert_eq!(pp.glass_type, GlassType::PressurePane);
        assert_eq!(pp.damage_space, DamageSpace::Planar);
        assert!(!pp.d3[0].random_rotation);
    }

    #[test]
    fn rules_reject_mismatched_and_out_of_range_sets() {
        let lam = GlassCrackSet::from_ron(LAMINATE).expect("laminate");
        let mut no_rules = lam.clone();
        no_rules.laminate = None;
        assert!(matches!(
            no_rules.validate(),
            Err(GlassCrackError::Invalid { .. })
        ));
        let mut both = lam.clone();
        both.pressure_pane = Some(PressurePaneRules::default());
        assert!(both.validate().is_err());
        let mut glitter = GlassCrackSet::from_ron(PRESSURE).expect("pressure");
        if let Some(rules) = glitter.pressure_pane.as_mut() {
            rules.chunks.size_m = (0.005, 0.01);
        }
        assert!(
            glitter.validate().is_err(),
            "glitter-sized chunks must be rejected"
        );
        let mut inf_size = lam.clone();
        inf_size.d1[0].size_m = (f32::INFINITY, f32::INFINITY);
        assert!(inf_size.validate().is_err(), "infinite stamp size");
        let mut nan_threshold = lam.clone();
        nan_threshold.escalation.d2 = f32::NAN;
        assert!(nan_threshold.validate().is_err(), "NaN escalation threshold");
        let mut empty = lam.clone();
        empty.d2.clear();
        assert!(empty.validate().is_err());
        let mut page = lam;
        page.page_size = 300;
        assert!(page.validate().is_err());
        let newer = LAMINATE.replace("format_version: 1", "format_version: 2");
        assert!(matches!(
            GlassCrackSet::from_ron(&newer),
            Err(GlassCrackError::Version { found: 2, .. })
        ));
    }

    #[test]
    fn escalation_levels() {
        let e = Escalation::default();
        assert_eq!(e.level_for(0.0), 0);
        assert_eq!(e.level_for(1.0), 1);
        assert_eq!(e.level_for(4.5), 2);
        assert_eq!(e.level_for(100.0), 3);
        let manual = Escalation {
            d1: 0.0,
            d2: 0.0,
            d3: 0.0,
        };
        assert_eq!(manual.level_for(100.0), 0);
    }

    fn blank(size: u32) -> StampPixels {
        let n = (size * size) as usize;
        StampPixels {
            size,
            mask: vec![0; n * 4],
            normal: vec![128; n * 2],
        }
    }

    #[test]
    fn cooked_round_trip_and_pixel_rules() {
        let manifest = GlassCrackSet::from_ron(LAMINATE).expect("laminate");
        let total = manifest.d1.len() + manifest.d2.len() + manifest.d3.len();
        let mut stamps: Vec<StampPixels> = (0..total).map(|_| blank(128)).collect();
        // A small hole at the pivot of the first D3 stamp: inside the 0.035 m limit.
        let d3 = manifest.d1.len() + manifest.d2.len();
        let centre = (64 * 128 + 64) * 4;
        stamps[d3].mask[centre + 2] = 255;
        let cooked = CookedCrackSet {
            manifest: manifest.clone(),
            stamps,
        };
        cooked.validate_pixels().expect("valid");
        let bytes = cooked.to_bytes().expect("bytes");
        assert_eq!(CookedCrackSet::from_bytes(&bytes).expect("decode"), cooked);
        assert!(CookedCrackSet::from_bytes(&bytes[..bytes.len() - 1]).is_err());

        // A hole in a D1 stamp is rejected.
        let mut d1_hole = cooked.clone();
        d1_hole.stamps[0].mask[centre + 2] = 200;
        assert!(d1_hole.validate_pixels().is_err());
        // A laminate D3 hole far from the pivot is rejected (0.6 m stamp: corner ≈ 0.42 m away).
        let mut far = cooked;
        far.stamps[d3].mask[2] = 255;
        assert!(far.validate_pixels().is_err());
    }

    #[test]
    fn fingerprint_lists_every_struct_and_enum() {
        let fp = glasscrack_schema_fingerprint();
        for needle in [
            "glass_type",
            "size_m",
            "crack_scatter",
            "max_hole_radius_m",
            "pane_gap_m",
            "pressure_pane",
            "uv1",
        ] {
            assert!(fp.contains(needle), "{needle} missing from {fp}");
        }
    }

    /// The JSON schema in docs/formats names exactly the serde fields and variants.
    #[test]
    fn json_schema_matches_the_serde_types() {
        use regia_core::schema::{serde_field_names, serde_variant_names};
        let schema: serde_json::Value =
            serde_json::from_str(include_str!("../../../docs/formats/glasscrack.schema.json"))
                .expect("schema json");
        let keys = |v: &serde_json::Value| -> Vec<String> {
            let mut k: Vec<String> = v["properties"]
                .as_object()
                .map(|o| o.keys().cloned().collect())
                .unwrap_or_default();
            k.sort();
            k
        };
        let sorted = |mut v: Vec<String>| {
            v.sort();
            v
        };
        let defs = &schema["$defs"];
        assert_eq!(keys(&schema), sorted(serde_field_names::<GlassCrackSet>()));
        assert_eq!(keys(&defs["stamp"]), sorted(serde_field_names::<CrackStamp>()));
        assert_eq!(
            keys(&defs["shading"]),
            sorted(serde_field_names::<CrackShading>())
        );
        assert_eq!(
            keys(&defs["escalation"]),
            sorted(serde_field_names::<Escalation>())
        );
        assert_eq!(
            keys(&defs["laminate"]),
            sorted(serde_field_names::<LaminateRules>())
        );
        assert_eq!(
            keys(&defs["pressurePane"]),
            sorted(serde_field_names::<PressurePaneRules>())
        );
        assert_eq!(
            keys(&defs["pressurePane"]["properties"]["chunks"]),
            sorted(serde_field_names::<ChunkBurst>())
        );
        let variants = |v: &serde_json::Value| -> Vec<String> {
            v["enum"]
                .as_array()
                .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                .unwrap_or_default()
        };
        assert_eq!(
            variants(&schema["properties"]["glass_type"]),
            serde_variant_names::<GlassType>()
        );
        assert_eq!(
            variants(&schema["properties"]["damage_space"]),
            serde_variant_names::<DamageSpace>()
        );
    }
}
