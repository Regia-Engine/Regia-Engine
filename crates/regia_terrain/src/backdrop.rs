// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (shell-regia/game/src/world/backdrop.rs, commit c9513623f), ADR-8012.
//! Far backdrop ring: a visual-only heightfield ring past the playable bounds (ADR-8459).
//!
//! [`extend_visual_map`] copies the playable (inner) [`TerrainMap`] sample for sample into the
//! centre of a larger map and fills a ring of `ring_texels` samples on every side, continued
//! from the inner map's edge heights: the edge eases to a plateau below the border's mean
//! height, ridge noise grows outward, and an optional far [`Landform`] fades in. The same
//! shared terrain draw shades the larger map, so the materials (layer albedo, macro, relief)
//! are the inner map's and there is no material seam.
//!
//! **Visual only.** The collider ([`TerrainMap::collision_heightfield`]), the navigation bake
//! and every simulation query keep the inner map: hand the inner map to them and the
//! [`BackdropBuild::visual_or`] map to the draw. The ring has no collision, no nav and no holes.
//!
//! **No seam.** The ring's first texel row on each side (one sample past the inner edge) holds
//! the inner edge sample exactly, so the drawn surface between the last inner sample and the
//! first ring sample is flat at the edge height: the rendered height difference across the seam
//! is 0, whatever the grid rounding of the larger map (`backdrop_seam_gap_zero`).
//!
//! A rectangular inner map ([`TerrainMap::with_world_depth`], ADR-8369) extends to a
//! rectangle with the same ring on every side; it is never padded to a square. A map within
//! `min_ring_texels` of `max_side` has no room for a ring and draws itself.
//!
//! [`BackdropRing`] holds the settings (ring width, size cap, the [`RingProfile`] as data, the
//! tiers that skip the ring: by default [`QualityTier::MobileLow`], Regia's lowest row).
//! [`build_visual_map`] applies them; its [`BackdropStatus`] is what the agent method
//! `terrain.backdrop_status` reports.

use regia_app::prelude::*;
use regia_render::QualityTier;
use serde::{Deserialize, Serialize};

use crate::TerrainMap;
use crate::sampler::TerrainMapError;

/// Default largest side of an extended map, samples: the size of a 2× ring around a 4096-sample
/// map, and a texture size every supported GPU takes.
pub const DEFAULT_MAX_SIDE: u32 = 8192;

/// Default fewest ring samples on a side worth building: a map closer than this to the size cap
/// has no ring.
pub const DEFAULT_MIN_RING_TEXELS: u32 = 64;

/// Default ring width as a fraction of the inner map's longer side, per side (a 4096-sample map
/// gets 2048 ring samples on each side, so the extended map is 8192 samples).
pub const DEFAULT_RING_FRACTION: f32 = 0.5;

/// Default distance over which the ring eases from the exact edge into the averaged border,
/// metres.
pub const DEFAULT_SEAM_EASE_M: f32 = 20.0;

/// Errors from the backdrop ring.
#[derive(Debug, thiserror::Error)]
pub enum BackdropError {
    /// The inner map is within `min_ring_texels` of `max_side`: no ring fits.
    #[error(
        "backdrop: no ring room for a {width}x{height} map (a ring needs {min_ring} samples a side and the extended map at most {max_side} samples a side)"
    )]
    NoRoom {
        /// Inner map columns.
        width: u32,
        /// Inner map rows.
        height: u32,
        /// The size cap of the settings.
        max_side: u32,
        /// The fewest ring samples a side of the settings.
        min_ring: u32,
    },
    /// A setting is out of range.
    #[error("backdrop settings: {0}")]
    BadSettings(String),
    /// A ring profile overlay does not parse or holds a bad value.
    #[error("backdrop profile: {0}")]
    BadProfile(String),
    /// The extended map could not be built.
    #[error("backdrop map: {0}")]
    Map(#[from] TerrainMapError),
}

/// The far landform laid over the ring.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Landform {
    /// Plain ring: the edge height easing to the plateau, plus ridge noise.
    #[default]
    None,
    /// Stepped, gullied cliff terraces rising to a plateau above the border.
    Terraces,
    /// Tilted, fractured polygonal blocks with grooves between them and long pressure ridges.
    ChaosBlocks,
}

impl Landform {
    /// Stable name (`none`, `terraces`, `chaos_blocks`).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Terraces => "terraces",
            Self::ChaosBlocks => "chaos_blocks",
        }
    }
}

/// The ring's shape: ridge wavelength and amplitude (metres), the plateau's height relative to
/// the border's mean (metres; negative is below), and the far landform. Data: a project keeps
/// one per map and layers a per-map [`RingProfileOverlay`] over a default.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RingProfile {
    /// Ridge noise wavelength, metres (> 0).
    pub ridge_wavelength_m: f32,
    /// Ridge noise amplitude at the outer edge, metres.
    pub ridge_amplitude_m: f32,
    /// Plateau height relative to the border's mean height, metres.
    pub plateau_drop_m: f32,
    /// Far landform.
    pub landform: Landform,
}

impl Default for RingProfile {
    fn default() -> Self {
        Self {
            ridge_wavelength_m: 420.0,
            ridge_amplitude_m: 5.0,
            plateau_drop_m: -20.0,
            landform: Landform::None,
        }
    }
}

/// The keys of a [`RingProfile`] a per-map file sets; a key it leaves out keeps the base value.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RingProfileOverlay {
    /// See [`RingProfile::ridge_wavelength_m`].
    pub ridge_wavelength_m: Option<f32>,
    /// See [`RingProfile::ridge_amplitude_m`].
    pub ridge_amplitude_m: Option<f32>,
    /// See [`RingProfile::plateau_drop_m`].
    pub plateau_drop_m: Option<f32>,
    /// See [`RingProfile::landform`].
    pub landform: Option<Landform>,
}

impl RingProfile {
    /// Checks the profile: finite values, a positive wavelength.
    ///
    /// # Errors
    ///
    /// [`BackdropError::BadProfile`] naming the bad key.
    pub fn validate(&self) -> Result<(), BackdropError> {
        if !(self.ridge_wavelength_m.is_finite() && self.ridge_wavelength_m > 0.0) {
            return Err(BackdropError::BadProfile(
                "ridge_wavelength_m must be positive".into(),
            ));
        }
        if !self.ridge_amplitude_m.is_finite() {
            return Err(BackdropError::BadProfile(
                "ridge_amplitude_m is not finite".into(),
            ));
        }
        if !self.plateau_drop_m.is_finite() {
            return Err(BackdropError::BadProfile("plateau_drop_m is not finite".into()));
        }
        Ok(())
    }

    /// This profile with the keys `overlay` sets layered over it.
    ///
    /// # Errors
    ///
    /// [`BackdropError::BadProfile`] when the result does not validate.
    pub fn overlaid(&self, overlay: &RingProfileOverlay) -> Result<RingProfile, BackdropError> {
        let out = RingProfile {
            ridge_wavelength_m: overlay.ridge_wavelength_m.unwrap_or(self.ridge_wavelength_m),
            ridge_amplitude_m: overlay.ridge_amplitude_m.unwrap_or(self.ridge_amplitude_m),
            plateau_drop_m: overlay.plateau_drop_m.unwrap_or(self.plateau_drop_m),
            landform: overlay.landform.unwrap_or(self.landform),
        };
        out.validate()?;
        Ok(out)
    }

    /// [`Self::overlaid`] with an overlay read from RON (`(ridge_amplitude_m: 30.0, landform:
    /// terraces)`; `Some(..)` is implied).
    ///
    /// # Errors
    ///
    /// [`BackdropError::BadProfile`] when the text does not parse or the result does not
    /// validate.
    pub fn overlaid_ron(&self, text: &str) -> Result<RingProfile, BackdropError> {
        let overlay: RingProfileOverlay = ron::Options::default()
            .with_default_extension(ron::extensions::Extensions::IMPLICIT_SOME)
            .from_str(text)
            .map_err(|e| BackdropError::BadProfile(e.to_string()))?;
        self.overlaid(&overlay)
    }

    /// [`Self::overlaid`] with an overlay read from JSON (`{"ridge_amplitude_m": 30.0}`; keys it
    /// does not know, such as a `_comment`, are ignored).
    ///
    /// # Errors
    ///
    /// [`BackdropError::BadProfile`] when the text does not parse or the result does not
    /// validate.
    pub fn overlaid_json(&self, text: &str) -> Result<RingProfile, BackdropError> {
        let overlay: RingProfileOverlay =
            serde_json::from_str(text).map_err(|e| BackdropError::BadProfile(e.to_string()))?;
        self.overlaid(&overlay)
    }
}

/// How wide the ring is on each side.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RingWidth {
    /// Ring samples per side as a fraction of the inner map's longer side (floored).
    InnerFraction(f32),
    /// Ring samples per side.
    Texels(u32),
}

impl Default for RingWidth {
    fn default() -> Self {
        Self::InnerFraction(DEFAULT_RING_FRACTION)
    }
}

/// Backdrop ring settings (resource). The defaults are the values the ring was tuned with.
#[derive(Resource, Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BackdropRing {
    /// Master switch.
    pub enabled: bool,
    /// Ring samples on each side.
    pub ring_width: RingWidth,
    /// Largest side of the extended map, samples (the ring shrinks to fit).
    pub max_side: u32,
    /// Fewest ring samples a side worth building; below this the map draws itself.
    pub min_ring_texels: u32,
    /// Distance past the seam over which the ring eases from the exact edge into the border
    /// average, metres.
    pub seam_ease_m: f32,
    /// Quality tiers that never build the ring (the inner map is drawn as is). Default: the
    /// lowest row, [`QualityTier::MobileLow`].
    pub off_tiers: Vec<QualityTier>,
    /// The ring's shape.
    pub profile: RingProfile,
}

impl Default for BackdropRing {
    fn default() -> Self {
        Self {
            enabled: true,
            ring_width: RingWidth::default(),
            max_side: DEFAULT_MAX_SIDE,
            min_ring_texels: DEFAULT_MIN_RING_TEXELS,
            seam_ease_m: DEFAULT_SEAM_EASE_M,
            off_tiers: vec![QualityTier::MobileLow],
            profile: RingProfile::default(),
        }
    }
}

impl BackdropRing {
    /// Checks the settings.
    ///
    /// # Errors
    ///
    /// [`BackdropError::BadSettings`] or [`BackdropError::BadProfile`] naming the bad value.
    pub fn validate(&self) -> Result<(), BackdropError> {
        match self.ring_width {
            RingWidth::InnerFraction(f) if !(f.is_finite() && f > 0.0) => {
                return Err(BackdropError::BadSettings(format!(
                    "ring_width inner_fraction must be positive, got {f}"
                )));
            }
            RingWidth::Texels(0) => {
                return Err(BackdropError::BadSettings("ring_width texels must be > 0".into()));
            }
            _ => {}
        }
        if self.min_ring_texels < 2 {
            return Err(BackdropError::BadSettings("min_ring_texels must be >= 2".into()));
        }
        if !(self.seam_ease_m.is_finite() && self.seam_ease_m >= 0.0) {
            return Err(BackdropError::BadSettings(
                "seam_ease_m must be finite and >= 0".into(),
            ));
        }
        self.profile.validate()
    }

    /// Whether the ring loads on `tier` (`None`: no tier known, the ring loads when enabled).
    #[must_use]
    pub fn loads_for_tier(&self, tier: Option<QualityTier>) -> bool {
        self.enabled && tier.is_none_or(|t| !self.off_tiers.contains(&t))
    }
}

/// The shape of the extended map for one inner map: the inner map copied 1:1 into the centre,
/// `ring_texels` of ring on every side at the inner map's sample spacing.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct BackdropSpec {
    /// Inner map `[columns, rows]`.
    pub inner_size: [u32; 2],
    /// Inner map `[world_width, world_depth]`, metres.
    pub inner_world: [f32; 2],
    /// Extended map `[columns, rows]`.
    pub extended_size: [u32; 2],
    /// Extended map `[world_width, world_depth]`, metres: the inner spacing, so the centre aligns.
    pub extended_world: [f32; 2],
    /// Ring samples on each side of the central block.
    pub ring_texels: u32,
}

impl BackdropSpec {
    /// The spec for `inner`, or `None` when no ring of at least `min_ring_texels` fits under
    /// `max_side`.
    #[must_use]
    pub fn for_inner(inner: &TerrainMap, ring: &BackdropRing) -> Option<BackdropSpec> {
        let (w, h) = (inner.width(), inner.height());
        if w < 2 || h < 2 {
            return None;
        }
        let longer = w.max(h);
        let wanted = match ring.ring_width {
            RingWidth::InnerFraction(f) => (f64::from(longer) * f64::from(f)).floor() as u32,
            RingWidth::Texels(t) => t,
        };
        let ring_texels = wanted.min(ring.max_side.saturating_sub(longer) / 2);
        if ring_texels < ring.min_ring_texels {
            return None;
        }
        let extended_size = [w + 2 * ring_texels, h + 2 * ring_texels];
        let inner_world = [inner.world_width(), inner.world_depth()];
        let extend = |world: f32, ext: u32, side: u32| world * (ext as f32 - 1.0) / (side as f32 - 1.0);
        Some(BackdropSpec {
            inner_size: [w, h],
            inner_world,
            extended_size,
            extended_world: [
                extend(inner_world[0], extended_size[0], w),
                extend(inner_world[1], extended_size[1], h),
            ],
            ring_texels,
        })
    }
}

/// What [`build_visual_map`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BackdropState {
    /// The ring was built; the draw takes the extended map.
    Built,
    /// The settings switch it off.
    Disabled,
    /// The quality tier skips it.
    TierOff,
    /// The inner map has no room for a ring under the size cap.
    NoRoom,
    /// No map was built yet (the host has not called [`build_visual_map`]).
    NotBuilt,
}

impl BackdropState {
    /// Stable name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Built => "built",
            Self::Disabled => "disabled",
            Self::TierOff => "tier_off",
            Self::NoRoom => "no_room",
            Self::NotBuilt => "not_built",
        }
    }
}

/// The backdrop's state for the agent method `terrain.backdrop_status` (resource; the host
/// inserts the status [`build_visual_map`] returns).
#[derive(Resource, Clone, Debug, PartialEq, Serialize)]
pub struct BackdropStatus {
    /// What the last build did.
    pub state: BackdropState,
    /// The tier the build was for, if known.
    pub tier: Option<QualityTier>,
    /// The inner (collider, nav, simulation) map `[columns, rows]`.
    pub inner_size: [u32; 2],
    /// The inner map's world extent, metres.
    pub inner_world: [f32; 2],
    /// The drawn map `[columns, rows]` (the inner map's unless the ring was built).
    pub visual_size: [u32; 2],
    /// The drawn map's world extent, metres.
    pub visual_world: [f32; 2],
    /// Ring samples on each side (0 without a ring).
    pub ring_texels: u32,
    /// The settings the build used.
    pub settings: BackdropRing,
}

impl BackdropStatus {
    /// The status before any build: `settings` and no sizes.
    #[must_use]
    pub fn not_built(settings: BackdropRing) -> Self {
        Self {
            state: BackdropState::NotBuilt,
            tier: None,
            inner_size: [0, 0],
            inner_world: [0.0, 0.0],
            visual_size: [0, 0],
            visual_world: [0.0, 0.0],
            ring_texels: 0,
            settings,
        }
    }

    fn without_ring(
        inner: &TerrainMap,
        ring: &BackdropRing,
        tier: Option<QualityTier>,
        state: BackdropState,
    ) -> Self {
        let size = [inner.width(), inner.height()];
        let world = [inner.world_width(), inner.world_depth()];
        Self {
            state,
            tier,
            inner_size: size,
            inner_world: world,
            visual_size: size,
            visual_world: world,
            ring_texels: 0,
            settings: ring.clone(),
        }
    }
}

/// The drawn map and the status of one [`build_visual_map`].
#[derive(Debug)]
pub struct BackdropBuild {
    /// The extended map when the ring was built; `None` draws the inner map itself.
    pub visual: Option<TerrainMap>,
    /// What the build did.
    pub status: BackdropStatus,
}

impl BackdropBuild {
    /// The map the terrain draw takes: the extended map, else `inner`. The collider, the nav
    /// bake and simulation queries always take `inner`.
    #[must_use]
    pub fn visual_or<'a>(&'a self, inner: &'a TerrainMap) -> &'a TerrainMap {
        self.visual.as_ref().unwrap_or(inner)
    }
}

/// The drawn map for `inner` under `ring` on `tier`: the extended map when the settings, the
/// tier and the size cap allow a ring, else none (the inner map is drawn as is).
///
/// # Errors
///
/// [`BackdropError::BadSettings`] / [`BackdropError::BadProfile`] for invalid settings, and
/// [`BackdropError::Map`] when the extended map cannot be built.
pub fn build_visual_map(
    inner: &TerrainMap,
    ring: &BackdropRing,
    tier: Option<QualityTier>,
) -> Result<BackdropBuild, BackdropError> {
    ring.validate()?;
    let off = |state| BackdropBuild {
        visual: None,
        status: BackdropStatus::without_ring(inner, ring, tier, state),
    };
    if !ring.enabled {
        return Ok(off(BackdropState::Disabled));
    }
    if !ring.loads_for_tier(tier) {
        return Ok(off(BackdropState::TierOff));
    }
    let Some(spec) = BackdropSpec::for_inner(inner, ring) else {
        return Ok(off(BackdropState::NoRoom));
    };
    let visual = extend_with_spec(inner, ring, &spec)?;
    Ok(BackdropBuild {
        visual: Some(visual),
        status: BackdropStatus {
            state: BackdropState::Built,
            tier,
            inner_size: spec.inner_size,
            inner_world: spec.inner_world,
            visual_size: spec.extended_size,
            visual_world: spec.extended_world,
            ring_texels: spec.ring_texels,
            settings: ring.clone(),
        },
    })
}

/// Builds the extended visual map: `inner` copied 1:1 into the centre, the ring continued from
/// its edge heights. The result keeps the inner map's height scale/offset and grid origin, so
/// the same shared draw shades it with no seam. Visual only: never collide or bake nav on it.
///
/// # Errors
///
/// [`BackdropError::NoRoom`] when no ring fits (see [`BackdropSpec::for_inner`]); the errors of
/// [`build_visual_map`] otherwise.
pub fn extend_visual_map(inner: &TerrainMap, ring: &BackdropRing) -> Result<TerrainMap, BackdropError> {
    ring.validate()?;
    let Some(spec) = BackdropSpec::for_inner(inner, ring) else {
        return Err(BackdropError::NoRoom {
            width: inner.width(),
            height: inner.height(),
            max_side: ring.max_side,
            min_ring: ring.min_ring_texels,
        });
    };
    extend_with_spec(inner, ring, &spec)
}

/// Deterministic 2-D integer hash -> [0,1).
fn hash2(x: i32, z: i32) -> f32 {
    let mut h = (x.wrapping_mul(374_761_393) ^ z.wrapping_mul(668_265_263)) as u32;
    h = (h ^ (h >> 13)).wrapping_mul(1_274_126_177);
    h ^= h >> 16;
    (h & 0xffff) as f32 / 65_536.0
}

/// Smooth (smoothstep-interpolated) value noise, [0,1).
fn value_noise(x: f32, z: f32) -> f32 {
    let xi = x.floor() as i32;
    let zi = z.floor() as i32;
    let fx = x - xi as f32;
    let fz = z - zi as f32;
    let u = fx * fx * (3.0 - 2.0 * fx);
    let v = fz * fz * (3.0 - 2.0 * fz);
    let a = hash2(xi, zi);
    let b = hash2(xi + 1, zi);
    let c = hash2(xi, zi + 1);
    let d = hash2(xi + 1, zi + 1);
    let ab = a + (b - a) * u;
    let cd = c + (d - c) * u;
    ab + (cd - ab) * v
}

/// Fractal (fBm) value noise, [0,1).
fn fbm(x: f32, z: f32, octaves: u32) -> f32 {
    let mut sum = 0.0;
    let mut amp = 1.0;
    let mut freq = 1.0;
    let mut norm = 0.0;
    for _ in 0..octaves {
        sum += value_noise(x * freq, z * freq) * amp;
        norm += amp;
        amp *= 0.5;
        freq *= 2.0;
    }
    sum / norm.max(1e-6)
}

/// Every sample of the inner map's one-sample border (each corner once per side it ends).
fn border_samples(inner: &TerrainMap) -> impl Iterator<Item = f32> + '_ {
    let (w, h) = (i64::from(inner.width()), i64::from(inner.height()));
    let rows = (0..w).flat_map(move |c| [inner.height_at_sample(c, 0), inner.height_at_sample(c, h - 1)]);
    let cols = (0..h).flat_map(move |r| [inner.height_at_sample(0, r), inner.height_at_sample(w - 1, r)]);
    rows.chain(cols)
}

/// Mean height of the inner map's one-sample border, metres.
fn border_mean(inner: &TerrainMap) -> f32 {
    let (sum, count) = border_samples(inner).fold((0.0f64, 0u64), |(s, n), h| (s + f64::from(h), n + 1));
    (sum / count.max(1) as f64) as f32
}

/// Highest height on the inner map's one-sample border, metres.
fn border_max(inner: &TerrainMap) -> f32 {
    border_samples(inner).fold(f32::NEG_INFINITY, f32::max)
}

/// The inner map's border as one closed loop in map-local coordinates (centre at 0; perimeter
/// coordinate `u`, metres: −Z side −X→+X, +X side −Z→+Z, +Z side +X→−X, −X side +Z→−Z), with a
/// running integral so the ring can take a box average of the border over any window in O(1).
/// The ring continues that average, widening with the distance past the seam: a single clamped
/// border sample would print every crater or ridge the border cuts as a straight streak running
/// out from the map, and the clamped corner value as a striped rectangle at each corner. The
/// window wraps round the corners, so they blend both sides.
struct BorderLoop {
    /// Half extents `[x, z]`, metres.
    half: [f32; 2],
    step: f32,
    /// `cum[i]` = integral of the border height from `u = 0` to `u = i * step`.
    cum: Vec<f64>,
}

impl BorderLoop {
    fn new(inner: &TerrainMap) -> Self {
        let half = [inner.world_width() * 0.5, inner.world_depth() * 0.5];
        let origin = inner.grid_origin();
        let step_x = 2.0 * half[0] / inner.width().saturating_sub(1).max(1) as f32;
        let step_z = 2.0 * half[1] / inner.height().saturating_sub(1).max(1) as f32;
        let step = step_x.min(step_z);
        let perimeter = 4.0 * (half[0] + half[1]);
        let n = ((perimeter / step).round() as usize).max(4);
        let step = perimeter / n as f32;
        let at = |[x, z]: [f32; 2]| f64::from(inner.sample_bilinear(x + origin[0], z + origin[1]));
        let mut cum = Vec::with_capacity(n + 1);
        cum.push(0.0f64);
        let mut prev = at(Self::point(half, 0.0));
        for i in 1..=n {
            let h = at(Self::point(half, i as f32 * step));
            let last = cum.last().copied().unwrap_or(0.0);
            cum.push(last + 0.5 * (prev + h) * f64::from(step));
            prev = h;
        }
        Self { half, step, cum }
    }

    fn perimeter(&self) -> f32 {
        4.0 * (self.half[0] + self.half[1])
    }

    /// Local `[x, z]` of perimeter coordinate `u` (wrapped).
    fn point(half: [f32; 2], u: f32) -> [f32; 2] {
        let (wx, wz) = (2.0 * half[0], 2.0 * half[1]);
        let u = u.rem_euclid(2.0 * (wx + wz));
        if u < wx {
            [-half[0] + u, -half[1]]
        } else if u < wx + wz {
            [half[0], -half[1] + (u - wx)]
        } else if u < 2.0 * wx + wz {
            [half[0] - (u - wx - wz), half[1]]
        } else {
            [-half[0], half[1] - (u - 2.0 * wx - wz)]
        }
    }

    /// Perimeter coordinate of a local point on the border.
    fn coord(&self, bx: f32, bz: f32) -> f32 {
        let [hx, hz] = self.half;
        let (wx, wz) = (2.0 * hx, 2.0 * hz);
        if (bz + hz).abs() < 1e-3 {
            bx + hx
        } else if (bx - hx).abs() < 1e-3 {
            wx + bz + hz
        } else if (bz - hz).abs() < 1e-3 {
            wx + wz + (hx - bx)
        } else {
            2.0 * wx + wz + (hz - bz)
        }
    }

    /// Integral of the border height from 0 to `u` (any real `u`, periodic).
    fn integral(&self, u: f32) -> f64 {
        let p = self.perimeter();
        let total = self.cum.last().copied().unwrap_or(0.0);
        let laps = (u / p).floor();
        let r = u - laps * p;
        let f = r / self.step;
        let i = (f.floor().max(0.0) as usize).min(self.cum.len() - 2);
        let t = f64::from(f - i as f32);
        f64::from(laps) * total + self.cum[i] + (self.cum[i + 1] - self.cum[i]) * t
    }

    /// Box average of the border over `u ± radius` (`radius` capped at half a lap).
    fn average(&self, u: f32, radius: f32) -> f32 {
        let r = radius.min(0.5 * self.perimeter());
        ((self.integral(u + r) - self.integral(u - r)) / f64::from(2.0 * r)) as f32
    }
}

/// Everything one ring sample's height depends on besides its position.
struct RingContext<'a> {
    inner: &'a TerrainMap,
    border: BorderLoop,
    /// Inner half extents `[x, z]`, metres (local).
    inner_half: [f32; 2],
    /// Inner map `[columns, rows]`.
    inner_size: [u32; 2],
    /// Extended map sample spacing `[x, z]`, metres.
    spacing: [f32; 2],
    /// Ring width, metres (where the falloff reaches the plateau).
    ring_w: f32,
    plateau: f32,
    border_max: f32,
    seam_ease_m: f32,
    profile: RingProfile,
}

impl RingContext<'_> {
    /// The border height the ring continues outward at distance `d` past the first ring row
    /// beside the local border point `(bx, bz)`, whose sample is `exact`: the border sample
    /// itself at the seam, then the border averaged along the edge over `±d`.
    fn edge_height(&self, bx: f32, bz: f32, exact: f32, d: f32) -> f32 {
        if d < self.border.step {
            return exact;
        }
        let avg = self.border.average(self.border.coord(bx, bz), d);
        // C0 at the seam: ease from the exact border into the average over the first metres.
        let k = if self.seam_ease_m > 0.0 {
            ((d - self.border.step) / self.seam_ease_m).clamp(0.0, 1.0)
        } else {
            1.0
        };
        exact + (avg - exact) * k * k * (3.0 - 2.0 * k)
    }

    /// Height of one ring sample at local `(x, z)`, `out` samples outside the inner rectangle
    /// on each axis, beside the inner border sample `edge`: the edge height continued outward
    /// with a smoothstep falloff to the plateau, plus ridge noise that is zero at the seam and
    /// grows outward, plus the far landform.
    ///
    /// Distances are measured from the first ring row (ADR-8459): that row (`out` 1 on an axis,
    /// at most 1 on the other) is at distance 0 and holds the inner edge sample exactly, so the
    /// drawn surface between the last inner sample and the first ring sample is flat at the edge
    /// height and the seam has no gap, whatever the larger map's coordinate rounding.
    fn ring_height(&self, x: f32, z: f32, edge: [u32; 2], out: [u32; 2]) -> f32 {
        let [hx, hz] = self.inner_half;
        let [cols, rows] = self.inner_size;
        // The border sample's local position; the last sample sits exactly on the half extent.
        let at = |i: u32, n: u32, half: f32| {
            if i + 1 >= n {
                half
            } else {
                -half + i as f32 * (2.0 * half / (n - 1) as f32)
            }
        };
        let (bx, bz) = (at(edge[0], cols, hx), at(edge[1], rows, hz));
        let exact = self
            .inner
            .height_at_sample(i64::from(edge[0]), i64::from(edge[1]));
        let dx = out[0].saturating_sub(1) as f32 * self.spacing[0];
        let dz = out[1].saturating_sub(1) as f32 * self.spacing[1];
        let d = (dx * dx + dz * dz).sqrt();
        let edge_h = self.edge_height(bx, bz, exact, d);
        let t = (d / self.ring_w).clamp(0.0, 1.0);
        let s = t * t * (3.0 - 2.0 * t); // smoothstep: C1 at the seam and the outer edge
        let base = edge_h + (self.plateau - edge_h) * s;
        let wl = self.profile.ridge_wavelength_m;
        let n = (fbm(x / wl, z / wl, 3) - 0.5) * 2.0; // -1..1
        let base = base + n * self.profile.ridge_amplitude_m * t;
        landform(self.profile.landform, x, z, t, base, self.border_max)
    }
}

/// The far landform added on top of the ring height. Faded in from the seam (zero there, C0), so
/// the playable edge is untouched.
/// - Terraces: the walls rise to a plateau ~60 m above the highest border sample through stepped,
///   gullied cliff terraces.
/// - Chaos blocks: tilted, fractured blocks 50-110 m across with grooves between them, plus long
///   pressure ridges.
fn landform(kind: Landform, x: f32, z: f32, t: f32, base: f32, border_max: f32) -> f32 {
    let fade = {
        // Fade in further out so the playable rectangle does not print as a seam in wide views.
        let u = ((t - 0.08) / 0.6).clamp(0.0, 1.0);
        u * u * (3.0 - 2.0 * u)
    };
    if fade <= 0.0 {
        return base;
    }
    match kind {
        Landform::Terraces => {
            let top = border_max + 60.0;
            let rise = base + (top - base) * fade;
            // terraces: soft steps 18-26 m tall, cliff bands between benches
            let step = 22.0 + 6.0 * (fbm(x / 900.0, z / 900.0, 2) - 0.5);
            let k = (rise / step).floor();
            let f = rise / step - k;
            let cliff = (f / 0.35).clamp(0.0, 1.0);
            let stepped = (k + cliff * cliff * (3.0 - 2.0 * cliff)) * step;
            // gullies cut back into the terraces
            let g = fbm(x / 70.0 + 13.0, z / 70.0, 3);
            let gully = ((g - 0.62) / 0.15).clamp(0.0, 1.0) * 14.0;
            let h = stepped - gully * fade;
            base + (h - base) * fade
        }
        Landform::ChaosBlocks => {
            // Irregular polygonal plates (warped Voronoi cells, ~90 m), each raised and tilted on
            // its own, with a groove along every cell border (an axis-aligned grid printed as
            // straight strips crossing at right angles).
            let cell = 90.0;
            let wx = x + 45.0 * (fbm(x / 260.0, z / 260.0, 3) - 0.5);
            let wz = z + 45.0 * (fbm(x / 260.0 + 17.0, z / 260.0 - 5.0, 3) - 0.5);
            let gx = (wx / cell).floor() as i32;
            let gz = (wz / cell).floor() as i32;
            let (mut d1, mut d2) = (f32::MAX, f32::MAX);
            let (mut id, mut fp) = ((0, 0), (0.0f32, 0.0f32));
            for oz in -1..=1 {
                for ox in -1..=1 {
                    let (ix, iz) = (gx + ox, gz + oz);
                    let px = (ix as f32 + 0.1 + 0.8 * hash2(ix, iz + 211)) * cell;
                    let pz = (iz as f32 + 0.1 + 0.8 * hash2(ix + 127, iz)) * cell;
                    let d = ((wx - px).powi(2) + (wz - pz).powi(2)).sqrt();
                    if d < d1 {
                        d2 = d1;
                        d1 = d;
                        id = (ix, iz);
                        fp = (px, pz);
                    } else if d < d2 {
                        d2 = d;
                    }
                }
            }
            let edge = 0.5 * (d2 - d1); // metres to the cell border
            let hb = (hash2(id.0, id.1) - 0.5) * 26.0;
            let tx = (hash2(id.0 + 91, id.1) - 0.5) * 0.25;
            let tz = (hash2(id.0, id.1 + 37) - 0.5) * 0.25;
            let gw = 4.0 + 3.0 * fbm(x / 120.0, z / 120.0, 2);
            let groove = (1.0 - (edge / gw).clamp(0.0, 1.0)).powi(2) * 7.0;
            let ridge = (1.0 - ((fbm(x / 220.0, z / 220.0, 3) - 0.5).abs() * 12.0).clamp(0.0, 1.0)) * 9.0;
            base + (hb + tx * (wx - fp.0) + tz * (wz - fp.1) - groove + ridge) * fade
        }
        Landform::None => base,
    }
}

/// Fills `heights` (rows of `width` samples) with `row_fn(row, out)`, bands of rows taken from a
/// shared queue by scoped worker threads and the caller: every sample is a pure function of its
/// position, so the result does not depend on the split. When no worker can be spawned the
/// caller fills every band.
fn fill_rows(heights: &mut [f32], width: usize, row_fn: impl Fn(u32, &mut [f32]) + Sync) {
    let width = width.max(1);
    let rows = heights.len() / width;
    let threads = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .clamp(1, 16);
    let band = rows.div_ceil(threads * 4).max(1);
    let queue = std::sync::Mutex::new(heights.chunks_mut(band * width).enumerate().collect::<Vec<_>>());
    let work = || {
        loop {
            let next = queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop();
            let Some((i, chunk)) = next else { break };
            for (j, out) in chunk.chunks_mut(width).enumerate() {
                row_fn((i * band + j) as u32, out);
            }
        }
    };
    std::thread::scope(|s| {
        for _ in 1..threads {
            if std::thread::Builder::new()
                .name("regia-backdrop".into())
                .spawn_scoped(s, work)
                .is_err()
            {
                tracing::warn!(target: "regia_terrain", "backdrop: worker thread not spawned; the caller fills the rest");
                break;
            }
        }
        work();
    });
}

fn extend_with_spec(
    inner: &TerrainMap,
    ring: &BackdropRing,
    spec: &BackdropSpec,
) -> Result<TerrainMap, BackdropError> {
    let _span = tracing::info_span!(target: "regia_terrain", "backdrop_extend", w = spec.extended_size[0], h = spec.extended_size[1]).entered();
    let [ew, eh] = spec.extended_size;
    let [world_x, world_z] = spec.extended_world;
    let [iw, ih] = spec.inner_size;
    let ring_texels = spec.ring_texels;
    let half = [world_x * 0.5, world_z * 0.5];
    let spacing = [world_x / (ew - 1) as f32, world_z / (eh - 1) as f32];
    let inner_half = [inner.world_width() * 0.5, inner.world_depth() * 0.5];
    // Distances start at the first ring row, so the falloff reaches the plateau at the outer edge
    // one row sooner than the full ring width.
    let ring_w = ((ring_texels - 1) as f32 * spacing[0]).min((ring_texels - 1) as f32 * spacing[1]);
    let ctx = RingContext {
        inner,
        border: BorderLoop::new(inner),
        inner_half,
        inner_size: [iw, ih],
        spacing,
        ring_w,
        plateau: border_mean(inner) + ring.profile.plateau_drop_m,
        border_max: border_max(inner),
        seam_ease_m: ring.seam_ease_m,
        profile: ring.profile.clone(),
    };
    let mut heights = vec![0.0f32; ew as usize * eh as usize];
    fill_rows(&mut heights, ew as usize, |row, out| {
        let z = -half[1] + row as f32 * spacing[1];
        for col in 0..ew {
            let x = -half[0] + col as f32 * spacing[0];
            // Inner indices of this sample, clamped to the inner map, and how far outside it is.
            let (ic, ir) = (
                i64::from(col) - i64::from(ring_texels),
                i64::from(row) - i64::from(ring_texels),
            );
            let (cc, rc) = (ic.clamp(0, i64::from(iw) - 1), ir.clamp(0, i64::from(ih) - 1));
            let outside = [(ic - cc).unsigned_abs() as u32, (ir - rc).unsigned_abs() as u32];
            out[col as usize] = if outside == [0, 0] {
                // Central block: the inner sample, exactly (no resample).
                inner.height_at_sample(cc, rc)
            } else {
                ctx.ring_height(x, z, [cc as u32, rc as u32], outside)
            };
        }
    });
    let mut map = TerrainMap::from_heights(
        ew,
        eh,
        heights,
        world_x,
        inner.height_scale(),
        inner.height_offset(),
    )?;
    if world_z != world_x {
        map = map.with_world_depth(world_z)?;
    }
    if inner.grid_origin() != [0.0, 0.0] {
        map = map.with_grid_origin(inner.grid_origin())?;
    }
    Ok(map)
}

#[cfg(test)]
mod tests;
