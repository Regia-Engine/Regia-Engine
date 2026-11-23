// SPDX-License-Identifier: MPL-2.0
//! Baked diffuse GI volume (`BakedGiVolume`, `.fgiv`): per-probe incident radiance as L1
//! spherical harmonics, split into separable light components, plus octahedral distance moments
//! for visibility. Baked offline by `regia_pt` (`regia bake-gi`), sampled at runtime by the
//! renderer (`docs/research/strategic-review-20261012.md` § 3, `docs/roadmap-post-alpha.md`
//! item 3).
//!
//! ## Layout
//!
//! Two probe sets share one probe index space:
//!
//! - [`TerrainLayer`]: a 2.5D grid that follows a heightfield. Column `(i, j)` stands on
//!   [`TerrainLayer::ground`] and holds one probe per [`TerrainLayer::levels`] height above it.
//!   The format's default levels are 1, 2, 4, 8, 16 m. Validation requires a non-empty list of
//!   at most [`MAX_TERRAIN_LEVELS`] (8) finite, strictly ascending heights and a positive column
//!   spacing. It does not require log steps or a 1–32 m band. Probe `((j·nx + i)·levels + l)`.
//! - [`DetailBrick`]: regular 3-D grids around static set pieces, after the terrain probes, each
//!   at [`DetailBrick::first_probe`]. Spacing is whatever the bake wrote; it must be finite and
//!   positive. Probe `first + (z·ny + y)·nx + x`.
//!
//! The runtime looks up the terrain layer trilinearly (XZ and the vertical band) and lets a
//! brick override it where present.
//!
//! ## Per-probe encoding (format versions 1 and 2)
//!
//! - **Radiance SH**, one 16-byte record per component ([`GiComponent`]): L0 per channel as
//!   RGB9E5 (`EXT_texture_shared_exponent`) of `L00 · exposure`, then the nine L1 coefficients
//!   (`[channel][x, y, z]`) as snorm8 of `L1 / (√3 · L00)` (non-negative radiance bounds the
//!   ratio to ±1), three bytes of padding. Coefficients are of incident *radiance* in the
//!   real SH basis `Y00 = 0.282095`, `Y1m = 0.488603 · (x, y, z)`; [`irradiance`] applies the
//!   clamped-cosine convolution (Ramamoorthi & Hanrahan 2001).
//! - **Distance moments**: `res × res` octahedral texels ([`oct_decode`]) of the mean and mean
//!   squared first-hit distance, unorm16 over [`BakedGiVolume::max_distance`] and its square.
//! - **Meta**: the relocation offset (f16 × 3, metres) and [`ProbeFlags`] bits (u16).
//!
//! The file is `FGIV`, the format version, the header length, a postcard [`VolumeHeader`],
//! padding to 16 bytes, then four raw little-endian sections (meta, SH, moments, ground) that a
//! later loader can map without copying. A blake3 hash of the sections guards against
//! truncation and corruption.
//!
//! Version 2 adds a flags word to every [`DetailBrick`] ([`BrickFlags`]: an interior brick keeps
//! the terrain layer's outdoor light out). Version 1 files still read, with every brick's flags
//! 0, and look up exactly as before.
//!
//! Version 3 (ADR-1386) adds the list of emitters the bake traced ([`BakedEmitter`], their world
//! bounds) at the end of the header: the [`GiComponent::Emissive`] light of exactly those
//! emitters is in the volume, so a renderer that also draws emissive surfaces as area lights
//! leaves those out where the volume covers. Version 1 and 2 files still read, with
//! [`BakedGiVolume::emitters`] `None` (not recorded).
//!
//! Version 4 (ADR-2618) adds the split section at the end of the header
//! ([`crate::baked_gi_split::VolumeSplit`]): a layout above [`PORTABLE_VOLUME_BYTES`] bakes as
//! up to [`crate::baked_gi_split::MAX_SPLIT_PARTS`] part files, each a whole volume of its own
//! that names its index, its neighbors and the overlap band it shares with them. Version 1 to 3
//! files still read, with [`BakedGiVolume::split`] `None` (one unsplit volume).
//!
//! Version 5 (ADR-3237) lets a split part hold a crop of a brick that crosses a cut: each
//! [`crate::baked_gi_split::SplitBrick`] gains the unsplit brick's counts, the crop offset and the
//! cells the part owns. Only the split section changed: an unsplit volume's header encodes the same
//! bytes as in version 4 (the version word differs). Version 4 parts read with no brick cropped.

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use crate::baked_gi_split::VolumeSplit;

/// Magic bytes of a baked GI volume file.
pub const BAKED_GI_MAGIC: [u8; 4] = *b"FGIV";
/// Current format version (5: cropped bricks in a split part, ADR-3237; 4: the split section; 3: the
/// baked emitter list; 2: per-brick flags).
pub const BAKED_GI_VERSION: u32 = 5;
/// Oldest format version this build reads (1: no brick flags, read as 0).
pub const BAKED_GI_MIN_VERSION: u32 = 1;
/// Separable light components per probe ([`GiComponent`]).
pub const GI_COMPONENTS: usize = 4;
/// Bytes of one component's SH record.
pub const SH_RECORD_BYTES: usize = 16;
/// Bytes of one probe's meta record.
pub const META_RECORD_BYTES: usize = 8;
/// Bytes of one moment texel.
pub const MOMENT_TEXEL_BYTES: usize = 4;
/// Most vertical levels of a [`TerrainLayer`].
pub const MAX_TERRAIN_LEVELS: usize = 8;
/// Bytes of one brick's entry in the renderer's brick table (12 words after the sections and the
/// ground heights; [`VolumeLayout::runtime_buffer_bytes`]).
pub const RUNTIME_BRICK_BYTES: u64 = 48;
/// Largest [`VolumeLayout::runtime_buffer_bytes`] `regia bake-gi` writes: 128 MiB, the
/// `max_storage_buffer_binding_size` of wgpu's default limits and of the renderer's mobile
/// minimum-spec profile, so every supported device can install the volume (ADR-357).
pub const PORTABLE_VOLUME_BYTES: u64 = 128 << 20;
/// `Y00` of the real SH basis.
pub const SH_Y00: f32 = 0.282_094_8;
/// `Y1m / (x, y, z)` of the real SH basis.
pub const SH_Y1: f32 = 0.488_602_52;
/// [`VolumeHeader::sh_encoding`] of format version 1.
pub const SH_ENCODING_RGB9E5_SNORM8: u32 = 1;
/// [`VolumeHeader::moment_encoding`] of format version 1.
pub const MOMENT_ENCODING_UNORM16: u32 = 1;

/// L1 radiance SH of one component: `[channel][L00, L1x, L1y, L1z]`.
pub type ShRgb = [[f32; 4]; 3];

/// The separable light components of a probe; each is linear in its light, so the host may
/// rescale it after the bake ([`BakeLights`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum GiComponent {
    /// Sun light after at least one bounce (the sun seen directly stays live).
    SunBounce = 0,
    /// Sky light, seen directly (sky visibility) and bounced.
    Sky = 1,
    /// Sky bodies (planet-shine fill), shadowed, seen directly and bounced.
    Fill = 2,
    /// Static emissive surfaces, seen directly and bounced (local lights stay live).
    Emissive = 3,
}

impl GiComponent {
    /// All components in storage order.
    pub const ALL: [Self; GI_COMPONENTS] = [Self::SunBounce, Self::Sky, Self::Fill, Self::Emissive];

    /// Lower-case name (`sun_bounce`, `sky`, `fill`, `emissive`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::SunBounce => "sun_bounce",
            Self::Sky => "sky",
            Self::Fill => "fill",
            Self::Emissive => "emissive",
        }
    }
}

/// Bits of [`ProbeMeta::flags`].
pub struct ProbeFlags;

impl ProbeFlags {
    /// The probe may be sampled.
    pub const VALID: u16 = 1;
    /// The bake moved the probe off its nominal position ([`ProbeMeta::offset`]).
    pub const RELOCATED: u16 = 2;
    /// The probe is below the terrain surface (invalid).
    pub const UNDER_TERRAIN: u16 = 4;
    /// Too many of the probe's rays hit back faces: it sits inside geometry (invalid).
    pub const INSIDE_GEOMETRY: u16 = 8;
    /// Wall code toward the next probe along +X (bits 4–7, [`Self::wall_code`]).
    pub const WALL_X: u16 = 0x00f0;
    /// Wall code toward the next probe along +Y, the next level up in the terrain layer
    /// (bits 8–11).
    pub const WALL_Y: u16 = 0x0f00;
    /// Wall code toward the next probe along +Z (bits 12–15).
    pub const WALL_Z: u16 = 0xf000;
    /// Every wall bit. A volume baked before the wall test (ADR-366) has none, and its lookup
    /// is unchanged; a reader that predates the codes ignores them.
    pub const WALLS: u16 = Self::WALL_X | Self::WALL_Y | Self::WALL_Z;
    /// Wall positions per code: code `k` (1..=15) puts the wall at `(k − 0.5) / 15` of the
    /// segment.
    pub const WALL_STEPS: u16 = 15;

    /// The 4-bit wall code toward the next probe along grid `axis` (0 = X, 1 = Y or the
    /// terrain level, 2 = Z): 0 when the segment to it crosses no geometry, else 1..=15.
    #[must_use]
    pub const fn wall_code(flags: u16, axis: usize) -> u16 {
        (flags >> (4 + 4 * (axis % 3))) & 0xf
    }

    /// Where the segment toward the next probe along `axis` crosses geometry, as a fraction of
    /// the segment from this probe (the middle of the first and last crossing, to 1/30), or
    /// `None` when it crosses nothing.
    #[must_use]
    pub fn wall_at(flags: u16, axis: usize) -> Option<f32> {
        let code = Self::wall_code(flags, axis);
        if code == 0 {
            None
        } else {
            Some((f32::from(code) - 0.5) / f32::from(Self::WALL_STEPS))
        }
    }

    /// The flag bits of a wall at fraction `t` (clamped to 0..=1) of the segment toward the
    /// next probe along `axis`.
    #[must_use]
    pub fn wall(axis: usize, t: f32) -> u16 {
        let steps = f32::from(Self::WALL_STEPS);
        let bin = (t.clamp(0.0, 1.0) * steps).floor().min(steps - 1.0) as u16;
        (bin + 1) << (4 + 4 * (axis % 3))
    }
}

/// Bits of [`DetailBrick::flags`] (format version 2).
pub struct BrickFlags;

impl BrickFlags {
    /// An enclosed interior: inside the brick the terrain layer contributes nothing, and the
    /// brick does not blend out over its last cell toward the outdoor probes. The runtime picks
    /// an interior brick over any other brick holding the point, and tests the point pushed
    /// half a cell along its normal, so an outer wall reads the outdoor light and an inner wall
    /// the interior's.
    pub const INTERIOR: u32 = 1;
    /// Reserved (bit 1); a file that sets it, or any higher bit, is refused.
    pub const RESERVED: u32 = 2;
    /// Every bit this build knows.
    pub const KNOWN: u32 = Self::INTERIOR;
}

/// The terrain-following probe layer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TerrainLayer {
    /// World X, Z of column (0, 0).
    pub origin: [f32; 2],
    /// Column spacing along X and Z, metres.
    pub spacing: [f32; 2],
    /// Columns along X and Z.
    pub counts: [u32; 2],
    /// Probe heights above the ground, metres, ascending (at most [`MAX_TERRAIN_LEVELS`]).
    pub levels: Vec<f32>,
    /// Ground height of every column (`j · nx + i`), metres. Stored in the ground section.
    #[serde(skip)]
    pub ground: Vec<f32>,
}

impl TerrainLayer {
    /// Probes in the layer.
    #[must_use]
    pub fn probe_count(&self) -> usize {
        // Saturating: a hostile header's counts must not overflow (`validate` rejects the sum).
        (self.counts[0] as usize)
            .saturating_mul(self.counts[1] as usize)
            .saturating_mul(self.levels.len())
    }

    /// Probe index of column `(i, j)`, level `l`.
    #[must_use]
    pub fn index(&self, i: u32, j: u32, l: u32) -> usize {
        (j as usize * self.counts[0] as usize + i as usize) * self.levels.len() + l as usize
    }

    /// `(i, j, l)` of probe `index` (inverse of [`Self::index`]).
    #[must_use]
    pub fn coords(&self, index: usize) -> (u32, u32, u32) {
        let levels = self.levels.len().max(1);
        let l = (index % levels) as u32;
        let column = index / levels;
        let nx = self.counts[0].max(1) as usize;
        ((column % nx) as u32, (column / nx) as u32, l)
    }

    /// Nominal (unrelocated) position of probe `(i, j, l)`.
    #[must_use]
    pub fn nominal_position(&self, i: u32, j: u32, l: u32) -> [f32; 3] {
        let g = self
            .ground
            .get(j as usize * self.counts[0] as usize + i as usize)
            .copied()
            .unwrap_or(0.0);
        let h = self.levels.get(l as usize).copied().unwrap_or(0.0);
        [
            self.origin[0] + i as f32 * self.spacing[0],
            g + h,
            self.origin[1] + j as f32 * self.spacing[1],
        ]
    }

    /// Half the distance to the nearest neighbor of level `l` along each axis (X, Y, Z): how
    /// far relocation may move a probe and stay inside its cell.
    #[must_use]
    pub fn half_cell(&self, l: u32) -> [f32; 3] {
        let l = l as usize;
        let here = self.levels.get(l).copied().unwrap_or(0.0);
        let below = match l.checked_sub(1).and_then(|k| self.levels.get(k)) {
            Some(prev) => here - prev,
            None => here,
        };
        let above = self.levels.get(l + 1).map_or(below, |v| v - here);
        [
            0.5 * self.spacing[0],
            0.5 * below.min(above).max(0.0),
            0.5 * self.spacing[1],
        ]
    }

    fn validate(&self) -> Result<(), BakedGiError> {
        let columns = (self.counts[0] as usize).saturating_mul(self.counts[1] as usize);
        if self.levels.is_empty() || self.levels.len() > MAX_TERRAIN_LEVELS {
            return Err(BakedGiError::Layout(format!(
                "terrain layer has {} levels (1..={MAX_TERRAIN_LEVELS})",
                self.levels.len()
            )));
        }
        if self.levels.windows(2).any(|w| w[1] <= w[0]) || self.levels.iter().any(|v| !v.is_finite()) {
            return Err(BakedGiError::Layout(
                "terrain levels must be finite and ascending".into(),
            ));
        }
        if self.ground.len() != columns {
            return Err(BakedGiError::Layout(format!(
                "terrain layer ground has {} heights for {columns} columns",
                self.ground.len()
            )));
        }
        if !(self.spacing[0].is_finite()
            && self.spacing[1].is_finite()
            && self.spacing[0] > 0.0
            && self.spacing[1] > 0.0)
        {
            return Err(BakedGiError::Layout(
                "terrain spacing must be finite and positive".into(),
            ));
        }
        if self.ground.iter().any(|v| !v.is_finite()) {
            return Err(BakedGiError::Layout(
                "terrain ground heights must be finite".into(),
            ));
        }
        if self.origin.iter().any(|v| !v.is_finite()) {
            return Err(BakedGiError::Layout("terrain origin must be finite".into()));
        }
        Ok(())
    }
}

/// A regular grid of probes around static geometry.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct DetailBrick {
    /// World position of probe (0, 0, 0).
    pub origin: [f32; 3],
    /// Probe spacing per axis, metres.
    pub spacing: [f32; 3],
    /// Probes per axis.
    pub counts: [u32; 3],
    /// Index of probe (0, 0, 0) in the volume.
    pub first_probe: u32,
    /// [`BrickFlags`] bits (format version 2; 0 in a version 1 file).
    pub flags: u32,
}

impl DetailBrick {
    /// Probes in the brick; saturates at `usize::MAX` instead of wrapping, so an absurd grid
    /// reads as over any budget and fails [`VolumeLayout::validate`].
    #[must_use]
    pub fn probe_count(&self) -> usize {
        self.counts
            .iter()
            .fold(1usize, |acc, &c| acc.saturating_mul(c as usize))
    }

    /// Volume index of probe `(x, y, z)`.
    #[must_use]
    pub fn index(&self, x: u32, y: u32, z: u32) -> usize {
        self.first_probe as usize
            + (z as usize * self.counts[1] as usize + y as usize) * self.counts[0] as usize
            + x as usize
    }

    /// `(x, y, z)` of the brick's local probe `local` (inverse of [`Self::index`]).
    #[must_use]
    pub fn coords(&self, local: usize) -> (u32, u32, u32) {
        let nx = self.counts[0].max(1) as usize;
        let ny = self.counts[1].max(1) as usize;
        (
            (local % nx) as u32,
            ((local / nx) % ny) as u32,
            (local / (nx * ny)) as u32,
        )
    }

    /// Nominal position of probe `(x, y, z)`.
    #[must_use]
    pub fn nominal_position(&self, x: u32, y: u32, z: u32) -> [f32; 3] {
        [
            self.origin[0] + x as f32 * self.spacing[0],
            self.origin[1] + y as f32 * self.spacing[1],
            self.origin[2] + z as f32 * self.spacing[2],
        ]
    }

    /// Centre of the brick's box (halfway between its first and last probe).
    #[must_use]
    pub fn centre(&self) -> [f32; 3] {
        std::array::from_fn(|a| {
            self.origin[a] + 0.5 * self.counts[a].saturating_sub(1) as f32 * self.spacing[a]
        })
    }

    /// [`BrickFlags::INTERIOR`] is set.
    #[must_use]
    pub fn is_interior(&self) -> bool {
        self.flags & BrickFlags::INTERIOR != 0
    }
}

/// Metres by which a drawn emitter's bounds may differ from a [`BakedEmitter`]'s and still be
/// the same emitter ([`BakedEmitter::matches`]): float noise between the bake's and the
/// renderer's transforms, and a mesh LOD's slightly different extent.
pub const EMITTER_MATCH_SLACK_M: f32 = 0.02;
/// Share of a [`BakedEmitter`]'s bounds diagonal added to [`EMITTER_MATCH_SLACK_M`] (a larger
/// emitter's LODs differ more).
pub const EMITTER_MATCH_SLACK_SHARE: f32 = 0.02;

/// An emissive surface whose light the bake traced (format version 3, ADR-1386): the world
/// bounds of one placed mesh with an emitting material ([`Self::from_object_bounds`]: the
/// object-space bounds of the vertices its triangles use, under its object-to-world transform).
/// The bake and the renderer compute them with the same function from the same mesh and
/// transform, so a drawn emitter matches its record up to float noise. The volume's
/// [`GiComponent::Emissive`] holds this emitter's light, seen directly and bounced; a renderer
/// that also lights the scene with the emitter (an area light fitted to it) drops that light
/// where the volume covers, so it is not counted twice. Emitters added after the bake, moved
/// ones and dynamic ones match no record and keep their area lights.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct BakedEmitter {
    /// Minimum corner of the world bounds, metres.
    pub min: [f32; 3],
    /// Maximum corner of the world bounds, metres.
    pub max: [f32; 3],
}

impl BakedEmitter {
    /// The bounds of `points` (world space), `None` when there are none or one is not finite.
    #[must_use]
    pub fn from_points(points: impl IntoIterator<Item = [f32; 3]>) -> Option<Self> {
        let mut min = [f32::INFINITY; 3];
        let mut max = [f32::NEG_INFINITY; 3];
        let mut any = false;
        for p in points {
            if p.iter().any(|v| !v.is_finite()) {
                return None;
            }
            for ((lo, hi), v) in min.iter_mut().zip(max.iter_mut()).zip(p) {
                *lo = lo.min(v);
                *hi = hi.max(v);
            }
            any = true;
        }
        any.then_some(Self { min, max })
    }

    /// The world bounds of the object-space box `min`..`max` under `model` (column-major
    /// object-to-world, affine): the box around its eight transformed corners. `None` when a
    /// value is not finite. The one definition the bake and the renderer share.
    #[must_use]
    pub fn from_object_bounds(min: [f32; 3], max: [f32; 3], model: &[[f32; 4]; 4]) -> Option<Self> {
        Self::from_points((0..8u32).map(|k| {
            let c = [
                if k & 1 == 0 { min[0] } else { max[0] },
                if k & 2 == 0 { min[1] } else { max[1] },
                if k & 4 == 0 { min[2] } else { max[2] },
            ];
            std::array::from_fn(|r| {
                model[0][r] * c[0] + model[1][r] * c[1] + model[2][r] * c[2] + model[3][r]
            })
        }))
    }

    /// How far (metres, per corner and axis) a drawn emitter's bounds may differ from these
    /// and still match: [`EMITTER_MATCH_SLACK_M`] plus [`EMITTER_MATCH_SLACK_SHARE`] of the
    /// diagonal.
    #[must_use]
    pub fn slack(&self) -> f32 {
        let d: f32 = (0..3).map(|a| (self.max[a] - self.min[a]).powi(2)).sum();
        EMITTER_MATCH_SLACK_M + EMITTER_MATCH_SLACK_SHARE * d.sqrt()
    }

    /// `true` when a drawn emitter with world bounds `min`..`max` is this baked one: every
    /// corner coordinate within [`Self::slack`].
    #[must_use]
    pub fn matches(&self, min: [f32; 3], max: [f32; 3]) -> bool {
        let slack = self.slack();
        (0..3).all(|a| (self.min[a] - min[a]).abs() <= slack && (self.max[a] - max[a]).abs() <= slack)
    }

    fn is_valid(&self) -> bool {
        (0..3).all(|a| self.min[a].is_finite() && self.max[a].is_finite() && self.min[a] <= self.max[a])
    }
}

/// The light levels the volume was baked with. Each component is linear in its light: the
/// runtime scales [`GiComponent::SunBounce`] by `current_sun_lux / sun_illuminance`, and so on,
/// so look changes after the bake still apply.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct BakeLights {
    /// Sun illuminance at normal incidence, lux (0 = no sun).
    pub sun_illuminance: f32,
    /// Sun linear color.
    pub sun_color: [f32; 3],
    /// Unit vector toward the sun (the bake is valid for this direction only).
    pub sun_direction: [f32; 3],
    /// Sky radiance scale (the sky table's multiplier, or the uniform sky's luminance).
    pub sky_scale: f32,
    /// Sky-body fill illuminance at normal incidence, lux (sum over the bodies).
    pub fill_lux: f32,
    /// Emissive scale (1 = as authored).
    pub emissive_scale: f32,
}

impl Default for BakeLights {
    fn default() -> Self {
        Self {
            sun_illuminance: 0.0,
            sun_color: [1.0; 3],
            sun_direction: [0.0, 1.0, 0.0],
            sky_scale: 0.0,
            fill_lux: 0.0,
            emissive_scale: 1.0,
        }
    }
}

/// How the volume was baked (reproducibility).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BakeInfo {
    /// Random seed: the same scene, settings, seed and device give the same volume.
    pub seed: u32,
    /// Rays per probe per sample.
    pub rays_per_probe: u32,
    /// Samples (rotated ray sets) per probe.
    pub samples: u32,
    /// Maximum path vertices.
    pub max_bounces: u32,
    /// Hash of the traced scene.
    pub scene_hash: u64,
    /// The 3×3×3 visibility-weighted SH filter ran.
    pub filtered: bool,
}

/// One probe's decoded meta record.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ProbeMeta {
    /// Relocation offset from the nominal position, metres.
    pub offset: [f32; 3],
    /// [`ProbeFlags`] bits.
    pub flags: u16,
}

impl ProbeMeta {
    /// [`ProbeFlags::VALID`] is set.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.flags & ProbeFlags::VALID != 0
    }
}

/// One probe's full-precision bake result ([`BakedGiVolume::from_probes`]).
#[derive(Clone, Debug, PartialEq)]
pub struct ProbeData {
    /// Radiance SH per component ([`GiComponent`] order).
    pub sh: [ShRgb; GI_COMPONENTS],
    /// Octahedral (mean, mean²) first-hit distance per texel, metres.
    pub moments: Vec<[f32; 2]>,
    /// Offset and flags.
    pub meta: ProbeMeta,
}

/// Byte range of a section after the header.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SectionRef {
    /// Offset from the start of the file, on a 16-byte boundary. Nonempty sections must
    /// follow the declared header and must not overlap another nonempty section.
    pub offset: u64,
    /// Length in bytes.
    pub len: u64,
}

/// The postcard header of a `.fgiv` file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VolumeHeader {
    /// Probes in the volume.
    pub probe_count: u32,
    /// Terrain layer descriptor (its ground heights are in the ground section).
    pub terrain: Option<TerrainLayer>,
    /// Detail bricks.
    pub bricks: Vec<DetailBrick>,
    /// Bake light levels.
    pub lights: BakeLights,
    /// Bake settings.
    pub info: BakeInfo,
    /// Octahedral moment texels per edge.
    pub moment_resolution: u32,
    /// Distance clamp of the moments, metres.
    pub max_distance: f32,
    /// Encode multiplier per component.
    pub exposure: [f32; GI_COMPONENTS],
    /// [`SH_ENCODING_RGB9E5_SNORM8`].
    pub sh_encoding: u32,
    /// [`MOMENT_ENCODING_UNORM16`].
    pub moment_encoding: u32,
    /// Meta, SH, moments, ground.
    pub sections: [SectionRef; 4],
    /// blake3 of the four sections in order.
    pub payload_hash: [u8; 32],
    /// The emitters the bake traced (format version 3; `None` = not recorded, a version 1 or
    /// 2 file).
    pub emitters: Option<Vec<BakedEmitter>>,
    /// The split section (format version 4, ADR-2618): `Some` in each part file of a split
    /// volume, `None` in an unsplit volume and in every version 1 to 3 file.
    pub split: Option<VolumeSplit>,
}

/// The header of a format version 4 file: [`VolumeHeader`] with the split section's bricks
/// uncropped (ADR-2618).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct VolumeHeaderV4 {
    probe_count: u32,
    terrain: Option<TerrainLayer>,
    bricks: Vec<DetailBrick>,
    lights: BakeLights,
    info: BakeInfo,
    moment_resolution: u32,
    max_distance: f32,
    exposure: [f32; GI_COMPONENTS],
    sh_encoding: u32,
    moment_encoding: u32,
    sections: [SectionRef; 4],
    payload_hash: [u8; 32],
    emitters: Option<Vec<BakedEmitter>>,
    split: Option<crate::baked_gi_split::VolumeSplitV4>,
}

impl From<VolumeHeaderV4> for VolumeHeader {
    fn from(h: VolumeHeaderV4) -> Self {
        let split = h.split.map(|s| s.upgrade(&h.bricks));
        Self {
            probe_count: h.probe_count,
            terrain: h.terrain,
            bricks: h.bricks,
            lights: h.lights,
            info: h.info,
            moment_resolution: h.moment_resolution,
            max_distance: h.max_distance,
            exposure: h.exposure,
            sh_encoding: h.sh_encoding,
            moment_encoding: h.moment_encoding,
            sections: h.sections,
            payload_hash: h.payload_hash,
            emitters: h.emitters,
            split,
        }
    }
}

/// The header of a format version 3 file: [`VolumeHeader`] without the split section.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct VolumeHeaderV3 {
    probe_count: u32,
    terrain: Option<TerrainLayer>,
    bricks: Vec<DetailBrick>,
    lights: BakeLights,
    info: BakeInfo,
    moment_resolution: u32,
    max_distance: f32,
    exposure: [f32; GI_COMPONENTS],
    sh_encoding: u32,
    moment_encoding: u32,
    sections: [SectionRef; 4],
    payload_hash: [u8; 32],
    emitters: Option<Vec<BakedEmitter>>,
}

impl From<VolumeHeaderV3> for VolumeHeader {
    fn from(h: VolumeHeaderV3) -> Self {
        Self {
            probe_count: h.probe_count,
            terrain: h.terrain,
            bricks: h.bricks,
            lights: h.lights,
            info: h.info,
            moment_resolution: h.moment_resolution,
            max_distance: h.max_distance,
            exposure: h.exposure,
            sh_encoding: h.sh_encoding,
            moment_encoding: h.moment_encoding,
            sections: h.sections,
            payload_hash: h.payload_hash,
            emitters: h.emitters,
            split: None,
        }
    }
}

/// The header of a format version 2 file: [`VolumeHeader`] without the emitter list.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct VolumeHeaderV2 {
    probe_count: u32,
    terrain: Option<TerrainLayer>,
    bricks: Vec<DetailBrick>,
    lights: BakeLights,
    info: BakeInfo,
    moment_resolution: u32,
    max_distance: f32,
    exposure: [f32; GI_COMPONENTS],
    sh_encoding: u32,
    moment_encoding: u32,
    sections: [SectionRef; 4],
    payload_hash: [u8; 32],
}

impl From<VolumeHeaderV2> for VolumeHeader {
    fn from(h: VolumeHeaderV2) -> Self {
        Self {
            probe_count: h.probe_count,
            terrain: h.terrain,
            bricks: h.bricks,
            lights: h.lights,
            info: h.info,
            moment_resolution: h.moment_resolution,
            max_distance: h.max_distance,
            exposure: h.exposure,
            sh_encoding: h.sh_encoding,
            moment_encoding: h.moment_encoding,
            sections: h.sections,
            payload_hash: h.payload_hash,
            emitters: None,
            split: None,
        }
    }
}

/// A detail brick as format version 1 stores it (no flags).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct DetailBrickV1 {
    origin: [f32; 3],
    spacing: [f32; 3],
    counts: [u32; 3],
    first_probe: u32,
}

/// The header of a format version 1 file: [`VolumeHeader`] with flagless bricks.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct VolumeHeaderV1 {
    probe_count: u32,
    terrain: Option<TerrainLayer>,
    bricks: Vec<DetailBrickV1>,
    lights: BakeLights,
    info: BakeInfo,
    moment_resolution: u32,
    max_distance: f32,
    exposure: [f32; GI_COMPONENTS],
    sh_encoding: u32,
    moment_encoding: u32,
    sections: [SectionRef; 4],
    payload_hash: [u8; 32],
}

impl From<VolumeHeaderV1> for VolumeHeader {
    fn from(h: VolumeHeaderV1) -> Self {
        Self {
            probe_count: h.probe_count,
            terrain: h.terrain,
            bricks: h
                .bricks
                .into_iter()
                .map(|b| DetailBrick {
                    origin: b.origin,
                    spacing: b.spacing,
                    counts: b.counts,
                    first_probe: b.first_probe,
                    flags: 0,
                })
                .collect(),
            lights: h.lights,
            info: h.info,
            moment_resolution: h.moment_resolution,
            max_distance: h.max_distance,
            exposure: h.exposure,
            sh_encoding: h.sh_encoding,
            moment_encoding: h.moment_encoding,
            sections: h.sections,
            payload_hash: h.payload_hash,
            emitters: None,
            split: None,
        }
    }
}

/// The postcard bytes of `header` in format `version` (1 drops the brick flags, 1 and 2 the
/// emitter list, 1 to 3 the split section, 4 the split bricks' crop: a cropped part is a
/// [`BakedGiError::Layout`] error there).
fn encode_header(header: &VolumeHeader, version: u32) -> Result<Vec<u8>, BakedGiError> {
    if version == 4 {
        let split = match &header.split {
            Some(s) => Some(
                crate::baked_gi_split::VolumeSplitV4::of(s, &header.bricks).ok_or_else(|| {
                    BakedGiError::Layout("cropped split bricks need format version 5".into())
                })?,
            ),
            None => None,
        };
        let v4 = VolumeHeaderV4 {
            probe_count: header.probe_count,
            terrain: header.terrain.clone(),
            bricks: header.bricks.clone(),
            lights: header.lights,
            info: header.info,
            moment_resolution: header.moment_resolution,
            max_distance: header.max_distance,
            exposure: header.exposure,
            sh_encoding: header.sh_encoding,
            moment_encoding: header.moment_encoding,
            sections: header.sections,
            payload_hash: header.payload_hash,
            emitters: header.emitters.clone(),
            split,
        };
        return postcard::to_allocvec(&v4).map_err(BakedGiError::from);
    }
    let bytes = if version == 3 {
        let v3 = VolumeHeaderV3 {
            probe_count: header.probe_count,
            terrain: header.terrain.clone(),
            bricks: header.bricks.clone(),
            lights: header.lights,
            info: header.info,
            moment_resolution: header.moment_resolution,
            max_distance: header.max_distance,
            exposure: header.exposure,
            sh_encoding: header.sh_encoding,
            moment_encoding: header.moment_encoding,
            sections: header.sections,
            payload_hash: header.payload_hash,
            emitters: header.emitters.clone(),
        };
        postcard::to_allocvec(&v3)
    } else if version == 2 {
        let v2 = VolumeHeaderV2 {
            probe_count: header.probe_count,
            terrain: header.terrain.clone(),
            bricks: header.bricks.clone(),
            lights: header.lights,
            info: header.info,
            moment_resolution: header.moment_resolution,
            max_distance: header.max_distance,
            exposure: header.exposure,
            sh_encoding: header.sh_encoding,
            moment_encoding: header.moment_encoding,
            sections: header.sections,
            payload_hash: header.payload_hash,
        };
        postcard::to_allocvec(&v2)
    } else if version == 1 {
        let v1 = VolumeHeaderV1 {
            probe_count: header.probe_count,
            terrain: header.terrain.clone(),
            bricks: header
                .bricks
                .iter()
                .map(|b| DetailBrickV1 {
                    origin: b.origin,
                    spacing: b.spacing,
                    counts: b.counts,
                    first_probe: b.first_probe,
                })
                .collect(),
            lights: header.lights,
            info: header.info,
            moment_resolution: header.moment_resolution,
            max_distance: header.max_distance,
            exposure: header.exposure,
            sh_encoding: header.sh_encoding,
            moment_encoding: header.moment_encoding,
            sections: header.sections,
            payload_hash: header.payload_hash,
        };
        postcard::to_allocvec(&v1)
    } else {
        postcard::to_allocvec(header)
    };
    bytes.map_err(BakedGiError::from)
}

/// Errors building, reading or writing a [`BakedGiVolume`].
#[derive(Debug, Error)]
pub enum BakedGiError {
    /// Not a `.fgiv` file or cut short.
    #[error("not a baked GI volume (bad magic or truncated)")]
    BadMagic,
    /// A version this build does not read.
    #[error("unsupported baked GI volume version {found} (this build reads {supported})")]
    UnsupportedVersion {
        /// Version in the file.
        found: u32,
        /// Version this build reads.
        supported: u32,
    },
    /// The header did not decode.
    #[error("baked GI volume header: {0}")]
    Header(#[from] postcard::Error),
    /// A section does not match the header, or the payload hash differs.
    #[error("baked GI volume payload: {0}")]
    Payload(String),
    /// Probe counts, levels or sizes are inconsistent.
    #[error("baked GI volume layout: {0}")]
    Layout(String),
    /// File IO.
    #[error("baked GI volume io: {0}")]
    Io(#[from] std::io::Error),
}

/// The brick lookup grid derived from one volume's brick table.
///
/// Equality ignores it: the grid is not part of the file, and a round trip compares equal
/// whether or not either side has been read. A poisoned lock is recovered; the next read
/// rebuilds the grid.
#[derive(Default)]
pub(crate) struct BrickGridCache {
    slot: std::sync::Mutex<Option<(u64, crate::baked_gi_grid::BrickGrid)>>,
}

impl Clone for BrickGridCache {
    fn clone(&self) -> Self {
        let guard = self
            .slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self {
            slot: std::sync::Mutex::new(guard.clone()),
        }
    }
}

impl PartialEq for BrickGridCache {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl std::fmt::Debug for BrickGridCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let cached = self
            .slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some();
        f.debug_struct("BrickGridCache").field("cached", &cached).finish()
    }
}

/// Blake3 of the fields [`crate::baked_gi_grid::BrickGrid::build`] reads. `first_probe` is not
/// one of them.
fn brick_table_key(bricks: &[DetailBrick]) -> u64 {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&(bricks.len() as u64).to_le_bytes());
    for brick in bricks {
        for value in brick.origin {
            hasher.update(&value.to_le_bytes());
        }
        for value in brick.spacing {
            hasher.update(&value.to_le_bytes());
        }
        for value in brick.counts {
            hasher.update(&value.to_le_bytes());
        }
        hasher.update(&brick.flags.to_le_bytes());
    }
    let digest = *hasher.finalize().as_bytes();
    let mut key = [0u8; 8];
    key.copy_from_slice(&digest[..8]);
    u64::from_le_bytes(key)
}

/// A baked diffuse GI volume (module docs).
#[derive(Clone, Debug, PartialEq)]
pub struct BakedGiVolume {
    /// The terrain-following layer (probes `0..terrain.probe_count()`).
    pub terrain: Option<TerrainLayer>,
    /// Detail bricks after the terrain probes.
    pub bricks: Vec<DetailBrick>,
    /// Bake light levels.
    pub lights: BakeLights,
    /// Bake settings.
    pub info: BakeInfo,
    /// Octahedral moment texels per edge.
    pub moment_resolution: u32,
    /// Distance clamp of the moments, metres.
    pub max_distance: f32,
    /// The emitters whose light [`GiComponent::Emissive`] holds (format version 3, ADR-1386):
    /// `Some` (possibly empty) when the bake recorded them, `None` for a volume read from a
    /// version 1 or 2 file, whose emissive light a renderer cannot attribute (it keeps every
    /// area light, as before). [`Self::from_probes`] leaves it `None`; the baker sets it.
    pub emitters: Option<Vec<BakedEmitter>>,
    /// The split section (format version 4, ADR-2618): `Some` when this volume is one part of a
    /// split volume ([`Self::split_parts`]), `None` for a whole volume.
    pub split: Option<VolumeSplit>,
    pub(crate) exposure: [f32; GI_COMPONENTS],
    pub(crate) meta: Vec<u8>,
    pub(crate) sh: Vec<u8>,
    pub(crate) moments: Vec<u8>,
    /// Brick lookup grid for this table. Empty until the first read; rebuilt when the table's
    /// origin, spacing, counts or flags change.
    pub(crate) grid_cache: BrickGridCache,
}

/// The probe layout of a volume: terrain layer, bricks, moment resolution and distance clamp.
#[derive(Clone, Debug, PartialEq)]
pub struct VolumeLayout {
    /// The terrain-following layer.
    pub terrain: Option<TerrainLayer>,
    /// Detail bricks (their `first_probe` is assigned by [`Self::assign_brick_offsets`]).
    pub bricks: Vec<DetailBrick>,
    /// Octahedral moment texels per edge (4 for the terrain layer, 8 for detail).
    pub moment_resolution: u32,
    /// Distance clamp of the moments, metres.
    pub max_distance: f32,
}

impl VolumeLayout {
    /// Probes in the layout.
    #[must_use]
    pub fn probe_count(&self) -> usize {
        self.bricks.iter().fold(
            self.terrain.as_ref().map_or(0, TerrainLayer::probe_count),
            |acc, b| acc.saturating_add(b.probe_count()),
        )
    }

    /// Sets every brick's `first_probe` after the terrain probes and the bricks before it.
    pub fn assign_brick_offsets(&mut self) {
        let mut next = self.terrain.as_ref().map_or(0, TerrainLayer::probe_count);
        for b in &mut self.bricks {
            b.first_probe = next as u32;
            next = next.saturating_add(b.probe_count());
        }
    }

    /// Nominal position of probe `index` (`None` past the end).
    #[must_use]
    pub fn nominal_position(&self, index: usize) -> Option<[f32; 3]> {
        let terrain_count = self.terrain.as_ref().map_or(0, TerrainLayer::probe_count);
        if index < terrain_count {
            let t = self.terrain.as_ref()?;
            let (i, j, l) = t.coords(index);
            return Some(t.nominal_position(i, j, l));
        }
        self.bricks.iter().find_map(|b| {
            let first = b.first_probe as usize;
            (index >= first && index < first + b.probe_count()).then(|| {
                let (x, y, z) = b.coords(index - first);
                b.nominal_position(x, y, z)
            })
        })
    }

    /// Nominal position of every probe in index order.
    #[must_use]
    pub fn nominal_positions(&self) -> Vec<[f32; 3]> {
        let mut out = Vec::with_capacity(self.probe_count());
        if let Some(t) = &self.terrain {
            for index in 0..t.probe_count() {
                let (i, j, l) = t.coords(index);
                out.push(t.nominal_position(i, j, l));
            }
        }
        for b in &self.bricks {
            for local in 0..b.probe_count() {
                let (x, y, z) = b.coords(local);
                out.push(b.nominal_position(x, y, z));
            }
        }
        out
    }

    /// Half cell extent (relocation bound) of every probe in index order.
    #[must_use]
    pub fn half_cells(&self) -> Vec<[f32; 3]> {
        let mut out = Vec::with_capacity(self.probe_count());
        if let Some(t) = &self.terrain {
            for index in 0..t.probe_count() {
                out.push(t.half_cell(t.coords(index).2));
            }
        }
        for b in &self.bricks {
            let h = [0.5 * b.spacing[0], 0.5 * b.spacing[1], 0.5 * b.spacing[2]];
            out.extend(std::iter::repeat_n(h, b.probe_count()));
        }
        out
    }

    /// Grid neighbors of every probe: for each probe, the volume indices of the up to 26 probes
    /// of its 3×3×3 neighborhood in the same layer or brick, with their offset `(dx, dy, dz)`
    /// in grid steps (`dy` is the level step in the terrain layer).
    #[must_use]
    pub fn neighbors(&self, index: usize) -> Vec<(usize, [i32; 3])> {
        let mut out = Vec::with_capacity(26);
        let mut visit = |dims: [u32; 3], at: [u32; 3], idx: &dyn Fn(u32, u32, u32) -> usize| {
            for dz in -1i32..=1 {
                for dy in -1i32..=1 {
                    for dx in -1i32..=1 {
                        if dx == 0 && dy == 0 && dz == 0 {
                            continue;
                        }
                        let p = [
                            i64::from(at[0]) + i64::from(dx),
                            i64::from(at[1]) + i64::from(dy),
                            i64::from(at[2]) + i64::from(dz),
                        ];
                        if (0..3).all(|a| p[a] >= 0 && p[a] < i64::from(dims[a])) {
                            out.push((idx(p[0] as u32, p[1] as u32, p[2] as u32), [dx, dy, dz]));
                        }
                    }
                }
            }
        };
        let terrain_count = self.terrain.as_ref().map_or(0, TerrainLayer::probe_count);
        if index < terrain_count {
            if let Some(t) = &self.terrain {
                let (i, j, l) = t.coords(index);
                let dims = [t.counts[0], t.levels.len() as u32, t.counts[1]];
                visit(dims, [i, l, j], &|x, y, z| t.index(x, z, y));
            }
            return out;
        }
        for b in &self.bricks {
            let first = b.first_probe as usize;
            if index >= first && index < first + b.probe_count() {
                let (x, y, z) = b.coords(index - first);
                visit(b.counts, [x, y, z], &|x, y, z| b.index(x, y, z));
                break;
            }
        }
        out
    }

    /// Sets [`BrickFlags::INTERIOR`] on every brick whose [`DetailBrick::centre`] lies inside
    /// one of the boxes (`(min, max)`, metres, bounds included) and returns how many bricks
    /// carry the flag afterwards. Which enclosures count as interiors (an opening, a shaft open
    /// to the sky) is the bake's choice: pass only the boxes that should keep outdoor light out.
    pub fn flag_interior_bricks(&mut self, boxes: &[([f32; 3], [f32; 3])]) -> usize {
        for b in &mut self.bricks {
            let c = b.centre();
            if boxes
                .iter()
                .any(|(lo, hi)| (0..3).all(|a| c[a] >= lo[a] && c[a] <= hi[a]))
            {
                b.flags |= BrickFlags::INTERIOR;
            }
        }
        self.bricks.iter().filter(|b| b.is_interior()).count()
    }

    /// Bytes of the one storage binding the renderer uploads a volume of this layout into: the
    /// meta, SH and moment sections, one `f32` ground height per terrain column and a
    /// [`RUNTIME_BRICK_BYTES`] table entry per brick and the brick lookup grid
    /// ([`crate::baked_gi_grid::BrickGrid`]; at least 16). A device whose
    /// `max_storage_buffer_binding_size` is below it cannot use the volume; `regia bake-gi`
    /// refuses a layout above [`PORTABLE_VOLUME_BYTES`]. `regia_render`'s tests keep the two
    /// layouts in step.
    #[must_use]
    pub fn runtime_buffer_bytes(&self) -> u64 {
        let texels = u64::from(self.moment_resolution) * u64::from(self.moment_resolution);
        let per_probe =
            (META_RECORD_BYTES + GI_COMPONENTS * SH_RECORD_BYTES) as u64 + texels * MOMENT_TEXEL_BYTES as u64;
        let ground = self.terrain.as_ref().map_or(0, |t| t.ground.len() as u64);
        let grid = crate::baked_gi_grid::BrickGrid::build(&self.bricks).word_count() as u64 * 4;
        (self.probe_count() as u64 * per_probe
            + ground * 4
            + self.bricks.len() as u64 * RUNTIME_BRICK_BYTES
            + grid)
            .max(16)
    }

    /// Checks levels, counts, brick offsets, the moment resolution and the distance clamp.
    ///
    /// # Errors
    ///
    /// [`BakedGiError::Layout`] naming the first inconsistency.
    pub fn validate(&self) -> Result<(), BakedGiError> {
        if let Some(t) = &self.terrain {
            t.validate()?;
        }
        let mut next = self.terrain.as_ref().map_or(0, TerrainLayer::probe_count);
        for b in &self.bricks {
            if b.first_probe as usize != next {
                return Err(BakedGiError::Layout(format!(
                    "brick starts at probe {} but the previous probes end at {next}",
                    b.first_probe
                )));
            }
            if b.spacing.iter().any(|s| !s.is_finite() || *s <= 0.0) {
                return Err(BakedGiError::Layout(
                    "brick spacing must be finite and positive".into(),
                ));
            }
            if b.origin.iter().any(|v| !v.is_finite()) {
                return Err(BakedGiError::Layout("brick origin must be finite".into()));
            }
            if b.flags & !BrickFlags::KNOWN != 0 {
                return Err(BakedGiError::Layout(format!(
                    "brick flags {:#x} set bits this build does not know",
                    b.flags
                )));
            }
            next = next
                .checked_add(b.probe_count())
                .filter(|n| *n <= u32::MAX as usize)
                .ok_or_else(|| BakedGiError::Layout("probe count exceeds u32".into()))?;
        }
        if self.moment_resolution == 0 || self.moment_resolution > 32 {
            return Err(BakedGiError::Layout(format!(
                "moment resolution {} (1..=32)",
                self.moment_resolution
            )));
        }
        if !(self.max_distance > 0.0 && self.max_distance.is_finite()) {
            return Err(BakedGiError::Layout("max distance must be positive".into()));
        }
        if next > u32::MAX as usize {
            return Err(BakedGiError::Layout(format!("{next} probes exceed u32")));
        }
        Ok(())
    }
}

/// The exposure a volume carries must stay decodable: [`BakedGiVolume::from_bytes`] divides by
/// it, so a zero, negative, NaN, infinite or subnormal value makes lux infinite. Tools that
/// re-assemble a volume ([`crate::baked_gi_split`]) pass through the same check.
pub(crate) fn check_exposure(exposure: [f32; GI_COMPONENTS]) -> Result<(), BakedGiError> {
    for e in exposure {
        // The decode divides by the exposure, so a zero or subnormal one makes lux infinite.
        if !(e.is_finite() && e > 0.0 && (1.0 / e).is_finite()) {
            return Err(BakedGiError::Payload(format!(
                "exposure {e} is not a positive number"
            )));
        }
    }
    Ok(())
}

impl BakedGiVolume {
    /// Encodes full-precision bake results (`probes[i]` is probe `i` of `layout`).
    ///
    /// # Errors
    ///
    /// [`BakedGiError::Layout`] when the layout is inconsistent or the probe count or a
    /// probe's moment count does not match it.
    pub fn from_probes(
        layout: &VolumeLayout,
        lights: BakeLights,
        info: BakeInfo,
        probes: &[ProbeData],
    ) -> Result<Self, BakedGiError> {
        layout.validate()?;
        let n = layout.probe_count();
        if probes.len() != n {
            return Err(BakedGiError::Layout(format!(
                "{} probes for a layout of {n}",
                probes.len()
            )));
        }
        let texels = (layout.moment_resolution * layout.moment_resolution) as usize;
        let mut exposure = [1.0f32; GI_COMPONENTS];
        for (c, e) in exposure.iter_mut().enumerate() {
            let peak = probes
                .iter()
                .flat_map(|p| p.sh[c].iter().map(|ch| ch[0]))
                .filter(|v| v.is_finite())
                .fold(0.0f32, f32::max);
            *e = exposure_for(peak);
        }
        let mut meta = Vec::with_capacity(n * META_RECORD_BYTES);
        let mut sh = Vec::with_capacity(n * GI_COMPONENTS * SH_RECORD_BYTES);
        let mut moments = Vec::with_capacity(n * texels * MOMENT_TEXEL_BYTES);
        let far = layout.max_distance;
        for (i, p) in probes.iter().enumerate() {
            if p.moments.len() != texels {
                return Err(BakedGiError::Layout(format!(
                    "probe {i} has {} moment texels, the layout {texels}",
                    p.moments.len()
                )));
            }
            for o in p.meta.offset {
                meta.extend_from_slice(&f32_to_f16(o).to_le_bytes());
            }
            meta.extend_from_slice(&p.meta.flags.to_le_bytes());
            for (c, rgb) in p.sh.iter().enumerate() {
                sh.extend_from_slice(&encode_sh(rgb, exposure[c]));
            }
            for m in &p.moments {
                let mean = unorm16(m[0] / far);
                let sq = unorm16(m[1] / (far * far));
                moments.extend_from_slice(&mean.to_le_bytes());
                moments.extend_from_slice(&sq.to_le_bytes());
            }
        }
        Ok(Self {
            terrain: layout.terrain.clone(),
            bricks: layout.bricks.clone(),
            lights,
            info,
            moment_resolution: layout.moment_resolution,
            max_distance: far,
            emitters: None,
            split: None,
            exposure,
            meta,
            sh,
            moments,
            grid_cache: BrickGridCache::default(),
        })
    }

    /// Probes in the volume.
    #[must_use]
    pub fn probe_count(&self) -> usize {
        self.meta.len() / META_RECORD_BYTES
    }

    /// The probe layout.
    #[must_use]
    pub fn layout(&self) -> VolumeLayout {
        VolumeLayout {
            terrain: self.terrain.clone(),
            bricks: self.bricks.clone(),
            moment_resolution: self.moment_resolution,
            max_distance: self.max_distance,
        }
    }

    /// Encode multipliers per component.
    #[must_use]
    pub fn exposure(&self) -> [f32; GI_COMPONENTS] {
        self.exposure
    }

    /// Bricks with [`BrickFlags::INTERIOR`].
    #[must_use]
    pub fn interior_brick_count(&self) -> usize {
        self.bricks.iter().filter(|b| b.is_interior()).count()
    }

    /// The brick lookup grid, built once until the brick table's origin, spacing, counts or
    /// flags change.
    fn with_brick_grid<R>(&self, f: impl FnOnce(&crate::baked_gi_grid::BrickGrid) -> R) -> R {
        let key = brick_table_key(&self.bricks);
        let mut guard = self
            .grid_cache
            .slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.as_ref().is_none_or(|(cached, _)| *cached != key) {
            *guard = Some((key, crate::baked_gi_grid::BrickGrid::build(&self.bricks)));
        }
        match guard.as_ref() {
            Some((_, grid)) => f(grid),
            None => {
                let grid = crate::baked_gi_grid::BrickGrid::build(&self.bricks);
                f(&grid)
            }
        }
    }

    /// Words of this volume's [`crate::baked_gi_grid::BrickGrid`].
    ///
    /// The grid is built from the brick table the first time it is read and kept until a brick's
    /// origin, spacing, counts or flags change. `first_probe` is not part of the grid.
    #[must_use]
    pub fn brick_grid_word_count(&self) -> usize {
        self.with_brick_grid(crate::baked_gi_grid::BrickGrid::word_count)
    }

    /// Cells in this volume's brick lookup grid. See [`Self::brick_grid_word_count`].
    #[must_use]
    pub fn brick_grid_cell_count(&self) -> usize {
        self.with_brick_grid(crate::baked_gi_grid::BrickGrid::cell_count)
    }

    /// Brick references in this volume's brick lookup grid. See [`Self::brick_grid_word_count`].
    #[must_use]
    pub fn brick_grid_entry_count(&self) -> usize {
        self.with_brick_grid(crate::baked_gi_grid::BrickGrid::entry_count)
    }

    /// This volume's brick lookup grid as the words uploaded after the brick table.
    #[must_use]
    pub fn brick_grid_words(&self) -> Vec<u32> {
        self.with_brick_grid(crate::baked_gi_grid::BrickGrid::to_words)
    }

    /// Probes with at least one [`ProbeFlags::WALLS`] bit: the bake found geometry between
    /// them and a grid neighbor (0 for a volume baked without the wall test).
    #[must_use]
    pub fn wall_probe_count(&self) -> usize {
        self.meta
            .as_chunks::<META_RECORD_BYTES>()
            .0
            .iter()
            .filter(|m| u16::from_le_bytes([m[6], m[7]]) & ProbeFlags::WALLS != 0)
            .count()
    }

    /// The raw meta section (formats v1 and v2: [`META_RECORD_BYTES`] per probe), as stored in
    /// the file: a renderer uploads it unchanged.
    #[must_use]
    pub fn meta_bytes(&self) -> &[u8] {
        &self.meta
    }

    /// The raw SH section (formats v1 and v2: [`GI_COMPONENTS`] × [`SH_RECORD_BYTES`] per probe).
    #[must_use]
    pub fn sh_bytes(&self) -> &[u8] {
        &self.sh
    }

    /// The raw moment section (formats v1 and v2: `res²` × [`MOMENT_TEXEL_BYTES`] per probe).
    #[must_use]
    pub fn moment_bytes(&self) -> &[u8] {
        &self.moments
    }

    /// Decoded meta record of probe `i` (default when out of range).
    #[must_use]
    pub fn probe_meta(&self, i: usize) -> ProbeMeta {
        let Some(b) = self.meta.get(i * META_RECORD_BYTES..(i + 1) * META_RECORD_BYTES) else {
            return ProbeMeta::default();
        };
        let h = |k: usize| f16_to_f32(u16::from_le_bytes([b[k], b[k + 1]]));
        ProbeMeta {
            offset: [h(0), h(2), h(4)],
            flags: u16::from_le_bytes([b[6], b[7]]),
        }
    }

    /// World position of probe `i` (nominal + relocation offset).
    #[must_use]
    pub fn probe_position(&self, i: usize) -> [f32; 3] {
        let nominal = self.layout().nominal_position(i).unwrap_or([0.0; 3]);
        let o = self.probe_meta(i).offset;
        [nominal[0] + o[0], nominal[1] + o[1], nominal[2] + o[2]]
    }

    /// Decoded radiance SH of probe `i`, component `c` (zero when out of range).
    #[must_use]
    pub fn probe_sh(&self, i: usize, c: GiComponent) -> ShRgb {
        let at = (i * GI_COMPONENTS + c as usize) * SH_RECORD_BYTES;
        match self.sh.get(at..at + SH_RECORD_BYTES) {
            Some(b) => {
                let mut rec = [0u8; SH_RECORD_BYTES];
                rec.copy_from_slice(b);
                decode_sh(&rec, self.exposure[c as usize])
            }
            None => [[0.0; 4]; 3],
        }
    }

    /// Irradiance (lux per unit of the component's baked light) of probe `i`, component `c`,
    /// on a surface facing `n`.
    #[must_use]
    pub fn irradiance(&self, i: usize, c: GiComponent, n: [f32; 3]) -> [f32; 3] {
        irradiance(&self.probe_sh(i, c), n)
    }

    /// Decoded (mean, mean²) distance moments of probe `i` per texel, metres.
    #[must_use]
    pub fn probe_moments(&self, i: usize) -> Vec<[f32; 2]> {
        let texels = (self.moment_resolution * self.moment_resolution) as usize;
        let at = i * texels * MOMENT_TEXEL_BYTES;
        let Some(b) = self.moments.get(at..at + texels * MOMENT_TEXEL_BYTES) else {
            return Vec::new();
        };
        let far = self.max_distance;
        b.as_chunks::<4>()
            .0
            .iter()
            .map(|t| {
                [
                    f32::from(u16::from_le_bytes([t[0], t[1]])) / 65535.0 * far,
                    f32::from(u16::from_le_bytes([t[2], t[3]])) / 65535.0 * far * far,
                ]
            })
            .collect()
    }

    /// Chebyshev visibility of a point `dist` metres from probe `i` toward `dir` (1 = visible).
    #[must_use]
    pub fn visibility(&self, i: usize, dir: [f32; 3], dist: f32) -> f32 {
        let m = self.probe_moments(i);
        let (mean, sq) = sample_moments(&m, self.moment_resolution, dir);
        chebyshev(mean, sq, dist)
    }

    /// Serializes the volume in the current format ([`BAKED_GI_VERSION`], module docs).
    ///
    /// # Errors
    ///
    /// The header failed to encode.
    pub fn to_bytes(&self) -> Result<Vec<u8>, BakedGiError> {
        self.encode(BAKED_GI_VERSION)
    }

    /// Serializes the volume in format version 1, for a reader that predates brick flags
    /// (and the compatibility tests). The sections are the same bytes as [`Self::to_bytes`].
    ///
    /// # Errors
    ///
    /// [`BakedGiError::Layout`] when a brick has flags or the emitters are recorded (version 1
    /// cannot hold them), or the header failed to encode.
    pub fn to_bytes_v1(&self) -> Result<Vec<u8>, BakedGiError> {
        if self.bricks.iter().any(|b| b.flags != 0) {
            return Err(BakedGiError::Layout("brick flags need format version 2".into()));
        }
        if self.emitters.is_some() {
            return Err(BakedGiError::Layout(
                "the emitter list needs format version 3".into(),
            ));
        }
        if self.split.is_some() {
            return Err(BakedGiError::Layout(
                "the split section needs format version 4".into(),
            ));
        }
        self.encode(1)
    }

    /// Serializes the volume in format version 2, for a reader that predates the emitter list
    /// (and the compatibility tests). The sections are the same bytes as [`Self::to_bytes`].
    ///
    /// # Errors
    ///
    /// [`BakedGiError::Layout`] when the emitters are recorded (version 2 cannot hold them: a
    /// renderer would count them twice), or the header failed to encode.
    pub fn to_bytes_v2(&self) -> Result<Vec<u8>, BakedGiError> {
        if self.emitters.is_some() {
            return Err(BakedGiError::Layout(
                "the emitter list needs format version 3".into(),
            ));
        }
        if self.split.is_some() {
            return Err(BakedGiError::Layout(
                "the split section needs format version 4".into(),
            ));
        }
        self.encode(2)
    }

    /// Serializes the volume in format version 4, for a reader that predates cropped split bricks
    /// (and the migration tests). The sections are the same bytes as [`Self::to_bytes`].
    ///
    /// # Errors
    ///
    /// [`BakedGiError::Layout`] when the volume is a split part with a cropped brick (version 4
    /// cannot hold the crop, ADR-3237), or the header failed to encode.
    pub fn to_bytes_v4(&self) -> Result<Vec<u8>, BakedGiError> {
        self.encode(4)
    }

    /// Serializes the volume in format version 3, for a reader that predates the split section
    /// (and the compatibility tests). The sections are the same bytes as [`Self::to_bytes`].
    ///
    /// # Errors
    ///
    /// [`BakedGiError::Layout`] when the volume is a split part (version 3 cannot say so: a
    /// reader would light the part alone, dark past its band), or the header failed to encode.
    pub fn to_bytes_v3(&self) -> Result<Vec<u8>, BakedGiError> {
        if self.split.is_some() {
            return Err(BakedGiError::Layout(
                "the split section needs format version 4".into(),
            ));
        }
        self.encode(3)
    }

    fn encode(&self, version: u32) -> Result<Vec<u8>, BakedGiError> {
        let ground: Vec<u8> = self
            .terrain
            .as_ref()
            .map(|t| t.ground.iter().flat_map(|g| g.to_le_bytes()).collect())
            .unwrap_or_default();
        let bodies: [&[u8]; 4] = [&self.meta, &self.sh, &self.moments, &ground];
        let mut hasher = blake3::Hasher::new();
        for b in bodies {
            hasher.update(b);
        }
        let mut header = VolumeHeader {
            probe_count: self.probe_count() as u32,
            terrain: self.terrain.clone(),
            bricks: self.bricks.clone(),
            lights: self.lights,
            info: self.info,
            moment_resolution: self.moment_resolution,
            max_distance: self.max_distance,
            exposure: self.exposure,
            sh_encoding: SH_ENCODING_RGB9E5_SNORM8,
            moment_encoding: MOMENT_ENCODING_UNORM16,
            sections: [SectionRef::default(); 4],
            payload_hash: *hasher.finalize().as_bytes(),
            emitters: self.emitters.clone(),
            split: self.split.clone(),
        };
        // Section offsets depend on the header length, which depends on the offsets' varint
        // sizes: settle them (at most a few rounds).
        let mut header_bytes = encode_header(&header, version)?;
        for _ in 0..4 {
            let mut at = align16(12 + header_bytes.len()) as u64;
            for (s, b) in header.sections.iter_mut().zip(bodies) {
                *s = SectionRef {
                    offset: at,
                    len: b.len() as u64,
                };
                at = align16((at + b.len() as u64) as usize) as u64;
            }
            let next = encode_header(&header, version)?;
            let settled = next.len() == header_bytes.len();
            header_bytes = next;
            if settled {
                break;
            }
        }
        let mut out = Vec::new();
        out.extend_from_slice(&BAKED_GI_MAGIC);
        out.extend_from_slice(&version.to_le_bytes());
        out.extend_from_slice(&(header_bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(&header_bytes);
        for (s, b) in header.sections.iter().zip(bodies) {
            if out.len() as u64 > s.offset {
                return Err(BakedGiError::Payload("section offsets did not settle".into()));
            }
            out.resize(s.offset as usize, 0);
            out.extend_from_slice(b);
        }
        out.resize(align16(out.len()), 0);
        Ok(out)
    }

    /// Reads a volume written by [`Self::to_bytes`] (format version 5) or by an older build
    /// (version 4: split bricks never cropped; version 3: no split section, [`Self::split`] `None`; version 2: also no emitter list,
    /// [`Self::emitters`] `None`; version 1: also every brick's flags read as 0).
    ///
    /// # Errors
    ///
    /// Bad magic, an unsupported version, a header that does not decode, sections that do not
    /// match the header, a hash mismatch, or an inconsistent layout. Misaligned sections and
    /// nonempty sections overlapping the declared header or one another are
    /// [`BakedGiError::Payload`], as is a ground byte count different from terrain columns × 4.
    /// Aligned gaps, disjoint reordered sections and unused padding are accepted; empty
    /// sections may share an aligned in-file boundary.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, BakedGiError> {
        if bytes.len() < 12 || bytes[0..4] != BAKED_GI_MAGIC {
            return Err(BakedGiError::BadMagic);
        }
        let word = |at: usize| u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
        let version = word(4);
        if !(BAKED_GI_MIN_VERSION..=BAKED_GI_VERSION).contains(&version) {
            return Err(BakedGiError::UnsupportedVersion {
                found: version,
                supported: BAKED_GI_VERSION,
            });
        }
        let header_len = word(8) as usize;
        let header_bytes = 12usize
            .checked_add(header_len)
            .and_then(|end| bytes.get(12..end))
            .ok_or(BakedGiError::BadMagic)?;
        let header: VolumeHeader = match version {
            1 => postcard::from_bytes::<VolumeHeaderV1>(header_bytes)?.into(),
            2 => postcard::from_bytes::<VolumeHeaderV2>(header_bytes)?.into(),
            3 => postcard::from_bytes::<VolumeHeaderV3>(header_bytes)?.into(),
            4 => postcard::from_bytes::<VolumeHeaderV4>(header_bytes)?.into(),
            _ => postcard::from_bytes(header_bytes)?,
        };
        if let Some(bad) = header.emitters.iter().flatten().find(|e| !e.is_valid()) {
            return Err(BakedGiError::Payload(format!(
                "baked emitter with bounds {:?}..{:?}",
                bad.min, bad.max
            )));
        }
        if header.sh_encoding != SH_ENCODING_RGB9E5_SNORM8
            || header.moment_encoding != MOMENT_ENCODING_UNORM16
        {
            return Err(BakedGiError::Payload(format!(
                "unknown encodings sh {} moments {}",
                header.sh_encoding, header.moment_encoding
            )));
        }
        let section = |k: usize| -> Result<&[u8], BakedGiError> {
            let s = header.sections[k];
            let range = usize::try_from(s.offset).ok().zip(
                s.offset
                    .checked_add(s.len)
                    .and_then(|end| usize::try_from(end).ok()),
            );
            let data = range
                .and_then(|(start, end)| bytes.get(start..end))
                .ok_or_else(|| BakedGiError::Payload(format!("section {k} runs past the end of the file")))?;
            if !s.offset.is_multiple_of(16) {
                return Err(BakedGiError::Payload(format!(
                    "section {k} is not 16-byte aligned"
                )));
            }
            if s.len != 0 {
                if s.offset < (12 + header_len) as u64 {
                    return Err(BakedGiError::Payload(format!("section {k} overlaps the header")));
                }
                for (j, other) in header.sections[..k].iter().enumerate() {
                    if other.len != 0
                        && s.offset < other.offset.saturating_add(other.len)
                        && other.offset < s.offset.saturating_add(s.len)
                    {
                        return Err(BakedGiError::Payload(format!("sections {j} and {k} overlap")));
                    }
                }
            }
            Ok(data)
        };
        let (meta, sh, moments, ground) = (section(0)?, section(1)?, section(2)?, section(3)?);
        let mut hasher = blake3::Hasher::new();
        for b in [meta, sh, moments, ground] {
            hasher.update(b);
        }
        if *hasher.finalize().as_bytes() != header.payload_hash {
            return Err(BakedGiError::Payload(
                "payload hash mismatch (corrupt file)".into(),
            ));
        }
        let mut terrain = header.terrain.clone();
        if let Some(t) = &mut terrain {
            let columns = u64::from(t.counts[0]) * u64::from(t.counts[1]);
            let ground_bytes = columns.checked_mul(4);
            if ground_bytes != Some(ground.len() as u64) {
                return Err(BakedGiError::Payload(format!(
                    "ground section has {} bytes for {columns} columns",
                    ground.len()
                )));
            }
            t.ground = ground
                .as_chunks::<4>()
                .0
                .iter()
                .map(|b| f32::from_le_bytes(*b))
                .collect();
        } else if !ground.is_empty() {
            return Err(BakedGiError::Payload(
                "ground heights without a terrain layer".into(),
            ));
        }
        let layout = VolumeLayout {
            terrain,
            bricks: header.bricks.clone(),
            moment_resolution: header.moment_resolution,
            max_distance: header.max_distance,
        };
        layout.validate()?;
        check_exposure(header.exposure)?;
        if let Some(split) = &header.split {
            split.validate(&layout)?;
        }
        let n = layout.probe_count();
        let texels = (header.moment_resolution * header.moment_resolution) as usize;
        if n != header.probe_count as usize
            || meta.len() != n * META_RECORD_BYTES
            || sh.len() != n * GI_COMPONENTS * SH_RECORD_BYTES
            || moments.len() != n * texels * MOMENT_TEXEL_BYTES
        {
            return Err(BakedGiError::Payload(format!(
                "section sizes do not match {n} probes of {texels} moment texels"
            )));
        }
        Ok(Self {
            terrain: layout.terrain,
            bricks: layout.bricks,
            lights: header.lights,
            info: header.info,
            moment_resolution: header.moment_resolution,
            max_distance: header.max_distance,
            emitters: header.emitters,
            split: header.split,
            exposure: header.exposure,
            meta: meta.to_vec(),
            sh: sh.to_vec(),
            moments: moments.to_vec(),
            grid_cache: BrickGridCache::default(),
        })
    }

    /// Writes the volume to `path`.
    ///
    /// # Errors
    ///
    /// Encoding or file IO failed.
    pub fn write(&self, path: &std::path::Path) -> Result<(), BakedGiError> {
        std::fs::write(path, self.to_bytes()?)?;
        Ok(())
    }

    /// Reads a volume from `path`.
    ///
    /// # Errors
    ///
    /// File IO failed or see [`Self::from_bytes`].
    pub fn read(path: &std::path::Path) -> Result<Self, BakedGiError> {
        Self::from_bytes(&std::fs::read(path)?)
    }
}

fn align16(n: usize) -> usize {
    n.div_ceil(16) * 16
}

/// `floor(log2(x))` for a positive finite `x`, read from the bit field (libm `log2` is not
/// required to agree across hosts, and these exponents are quantized into the file bytes).
pub(crate) fn floor_log2(x: f32) -> i32 {
    let bits = x.to_bits();
    let exp = ((bits >> 23) & 0xff) as i32;
    if exp == 0 {
        // Subnormal: value = mant * 2^-149.
        let mant = bits & 0x007f_ffff;
        31 - mant.leading_zeros() as i32 - 149
    } else {
        exp - 127
    }
}

/// `2^e` exactly for `e` in the f32 range (`-149..=127`), from the bit field.
pub(crate) fn exp2_int(e: i32) -> f32 {
    if e >= -126 {
        f32::from_bits(((e.min(127) + 127) as u32) << 23)
    } else {
        f32::from_bits(1u32 << (e.max(-149) + 149))
    }
}

/// Power-of-two encode multiplier that maps `peak` to at most 32768 (RGB9E5 tops out at 65408).
fn exposure_for(peak: f32) -> f32 {
    if peak.is_nan() || peak <= 0.0 || !peak.is_finite() {
        return 1.0;
    }
    exp2_int(floor_log2(32768.0 / peak).clamp(-60, 60))
}

/// Irradiance on a surface facing `n` from L1 radiance SH: `π·Y00·L00 + (2π/3)·Y1·(L1·n)`
/// per channel, clamped at 0 (Ramamoorthi & Hanrahan 2001).
#[must_use]
pub fn irradiance(sh: &ShRgb, n: [f32; 3]) -> [f32; 3] {
    let a0 = std::f32::consts::PI * SH_Y00;
    let a1 = 2.0 * std::f32::consts::PI / 3.0 * SH_Y1;
    let mut out = [0.0f32; 3];
    for (o, c) in out.iter_mut().zip(sh) {
        *o = (a0 * c[0] + a1 * (c[1] * n[0] + c[2] * n[1] + c[3] * n[2])).max(0.0);
    }
    out
}

/// Octahedral encode of a direction to `[0, 1]²` (Cigolle et al. 2014; fold on −Z).
#[must_use]
pub fn oct_encode(dir: [f32; 3]) -> [f32; 2] {
    let l1 = dir[0].abs() + dir[1].abs() + dir[2].abs();
    if l1 <= 0.0 {
        return [0.5, 0.5];
    }
    let (mut u, mut v) = (dir[0] / l1, dir[1] / l1);
    if dir[2] < 0.0 {
        let sign = |x: f32| if x >= 0.0 { 1.0 } else { -1.0 };
        let (ou, ov) = (u, v);
        u = (1.0 - ov.abs()) * sign(ou);
        v = (1.0 - ou.abs()) * sign(ov);
    }
    [u * 0.5 + 0.5, v * 0.5 + 0.5]
}

/// Octahedral decode of `uv ∈ [0, 1]²` (inverse of [`oct_encode`]; `pt.wgsl` `oct_decode`).
#[must_use]
pub fn oct_decode(uv: [f32; 2]) -> [f32; 3] {
    let (mut x, mut y) = (uv[0] * 2.0 - 1.0, uv[1] * 2.0 - 1.0);
    let z = 1.0 - x.abs() - y.abs();
    if z < 0.0 {
        let sign = |v: f32| if v >= 0.0 { 1.0 } else { -1.0 };
        let (ox, oy) = (x, y);
        x = (1.0 - oy.abs()) * sign(ox);
        y = (1.0 - ox.abs()) * sign(oy);
    }
    let len = (x * x + y * y + z * z).sqrt().max(1e-20);
    [x / len, y / len, z / len]
}

/// Direction of the centre of texel `t` of a `res × res` octahedral map.
#[must_use]
pub fn texel_direction(t: usize, res: u32) -> [f32; 3] {
    let res_f = res.max(1) as f32;
    let x = (t % res.max(1) as usize) as f32;
    let y = (t / res.max(1) as usize) as f32;
    oct_decode([(x + 0.5) / res_f, (y + 0.5) / res_f])
}

/// Bilinear (mean, mean²) of `moments` (`res × res`, row-major) toward `dir`, clamped at the
/// map edges.
#[must_use]
pub fn sample_moments(moments: &[[f32; 2]], res: u32, dir: [f32; 3]) -> (f32, f32) {
    let r = res.max(1) as usize;
    if moments.len() < r * r {
        return (0.0, 0.0);
    }
    let uv = oct_encode(dir);
    let fx = (uv[0] * r as f32 - 0.5).clamp(0.0, (r - 1) as f32);
    let fy = (uv[1] * r as f32 - 0.5).clamp(0.0, (r - 1) as f32);
    let (x0, y0) = (fx.floor() as usize, fy.floor() as usize);
    let (x1, y1) = ((x0 + 1).min(r - 1), (y0 + 1).min(r - 1));
    let (tx, ty) = (fx - x0 as f32, fy - y0 as f32);
    let tap = |x: usize, y: usize| moments[y * r + x];
    let lerp = |a: [f32; 2], b: [f32; 2], t: f32| [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t];
    let m = lerp(
        lerp(tap(x0, y0), tap(x1, y0), tx),
        lerp(tap(x0, y1), tap(x1, y1), tx),
        ty,
    );
    (m[0], m[1])
}

/// Chebyshev upper bound on the probability that a point `dist` away is visible given the
/// distance moments (Donnelly & Lauritzen 2006, as in DDGI): 1 at or before the mean.
#[must_use]
pub fn chebyshev(mean: f32, mean_sq: f32, dist: f32) -> f32 {
    if dist <= mean {
        return 1.0;
    }
    // A floor on the variance: a texel that saw one distance has none, and the bound would be
    // a hard step (cf. `regia_render::gi`).
    let variance = (mean_sq - mean * mean).max(1e-4 * mean * mean + 1e-4);
    let d = dist - mean;
    (variance / (variance + d * d)).clamp(0.0, 1.0)
}

fn unorm16(v: f32) -> u16 {
    (v.clamp(0.0, 1.0) * 65535.0 + 0.5) as u16
}

fn snorm8(v: f32) -> u8 {
    ((v.clamp(-1.0, 1.0) * 127.0).round() as i8) as u8
}

fn from_snorm8(b: u8) -> f32 {
    (f32::from(b as i8) / 127.0).max(-1.0)
}

fn encode_sh(rgb: &ShRgb, exposure: f32) -> [u8; SH_RECORD_BYTES] {
    let l0 = [rgb[0][0] * exposure, rgb[1][0] * exposure, rgb[2][0] * exposure];
    let packed = rgb9e5(l0);
    let decoded = from_rgb9e5(packed);
    let mut out = [0u8; SH_RECORD_BYTES];
    out[0..4].copy_from_slice(&packed.to_le_bytes());
    let k = 3.0f32.sqrt();
    for ch in 0..3 {
        let base = decoded[ch] / exposure * k;
        for m in 0..3 {
            let ratio = if base > 0.0 { rgb[ch][m + 1] / base } else { 0.0 };
            out[4 + ch * 3 + m] = snorm8(if ratio.is_finite() { ratio } else { 0.0 });
        }
    }
    out
}

fn decode_sh(rec: &[u8; SH_RECORD_BYTES], exposure: f32) -> ShRgb {
    let l0 = from_rgb9e5(u32::from_le_bytes([rec[0], rec[1], rec[2], rec[3]]));
    let k = 3.0f32.sqrt();
    let mut out = [[0.0f32; 4]; 3];
    for ch in 0..3 {
        let base = l0[ch] / exposure;
        out[ch][0] = base;
        for m in 0..3 {
            out[ch][m + 1] = from_snorm8(rec[4 + ch * 3 + m]) * base * k;
        }
    }
    out
}

/// RGB9E5 shared-exponent encode (`EXT_texture_shared_exponent`, N = 9, B = 15, Emax = 31):
/// red in bits 0–8, green 9–17, blue 18–26, exponent 27–31. Negative and NaN inputs encode 0.
#[must_use]
pub fn rgb9e5(c: [f32; 3]) -> u32 {
    const N: i32 = 9;
    const B: i32 = 15;
    const MAX: f32 = 65408.0;
    let clamp = |v: f32| if v > 0.0 { v.min(MAX) } else { 0.0 };
    let (r, g, b) = (clamp(c[0]), clamp(c[1]), clamp(c[2]));
    let maxc = r.max(g).max(b);
    if maxc <= 0.0 {
        return 0;
    }
    let mut exp = floor_log2(maxc).max(-B - 1) + 1 + B;
    let scale = |e: i32| exp2_int(e - B - N);
    let maxs = (maxc / scale(exp) + 0.5).floor() as i32;
    if maxs == 1 << N {
        exp += 1;
    }
    let q = |v: f32| ((v / scale(exp) + 0.5).floor() as u32).min(511);
    q(r) | (q(g) << 9) | (q(b) << 18) | ((exp.clamp(0, 31) as u32) << 27)
}

/// RGB9E5 decode (inverse of [`rgb9e5`]).
#[must_use]
pub fn from_rgb9e5(v: u32) -> [f32; 3] {
    let exp = (v >> 27) as i32;
    let scale = exp2_int(exp - 15 - 9);
    [
        (v & 511) as f32 * scale,
        ((v >> 9) & 511) as f32 * scale,
        ((v >> 18) & 511) as f32 * scale,
    ]
}

/// IEEE 754 binary16 from f32, round to nearest even; overflow saturates to ±infinity.
#[must_use]
pub fn f32_to_f16(v: f32) -> u16 {
    let bits = v.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exp = ((bits >> 23) & 0xff) as i32;
    let mant = bits & 0x007f_ffff;
    if exp == 0xff {
        return sign | 0x7c00 | if mant != 0 { 0x0200 } else { 0 };
    }
    let e = exp - 127 + 15;
    if e >= 0x1f {
        return sign | 0x7c00;
    }
    if e <= 0 {
        if e < -10 {
            return sign;
        }
        let m = mant | 0x0080_0000;
        let shift = (14 - e) as u32;
        let half = 1u32 << (shift - 1);
        let rem = m & ((1u32 << shift) - 1);
        let mut out = m >> shift;
        if rem > half || (rem == half && out & 1 == 1) {
            out += 1;
        }
        return sign | out as u16;
    }
    let mut out = ((e as u32) << 10) | (mant >> 13);
    let rem = mant & 0x1fff;
    if rem > 0x1000 || (rem == 0x1000 && out & 1 == 1) {
        out += 1;
    }
    sign | out as u16
}

/// f32 from IEEE 754 binary16.
#[must_use]
pub fn f16_to_f32(h: u16) -> f32 {
    let sign = if h & 0x8000 != 0 { -1.0f32 } else { 1.0 };
    let exp = i32::from((h >> 10) & 0x1f);
    let mant = f32::from(h & 0x03ff);
    match exp {
        0 => sign * mant * exp2_int(-24),
        0x1f => {
            if mant == 0.0 {
                sign * f32::INFINITY
            } else {
                f32::NAN
            }
        }
        _ => sign * (1.0 + mant / 1024.0) * exp2_int(exp - 15),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn bit_field_log2_and_exp2_match_exact_powers() {
        for e in -149..=127 {
            let x = exp2_int(e);
            assert_eq!(floor_log2(x), e, "2^{e}");
            if e > -149 {
                assert_eq!(
                    floor_log2(f32::from_bits(x.to_bits() - 1)),
                    e - 1,
                    "just below 2^{e}"
                );
            }
        }
        assert_eq!(floor_log2(3.0), 1);
        assert_eq!(floor_log2(0.75), -1);
        assert_eq!(exp2_int(-9), 1.0 / 512.0);
    }

    use super::*;

    #[test]
    fn a_wrapping_probe_product_saturates_and_fails_validation() {
        // 2^22 per axis: the product is 2^66, which wraps to 0 in a 64-bit usize.
        let brick = DetailBrick {
            origin: [0.0; 3],
            spacing: [1.0; 3],
            counts: [1 << 22; 3],
            first_probe: 0,
            flags: 0,
        };
        assert_eq!(brick.probe_count(), usize::MAX);
        let layout = VolumeLayout {
            terrain: None,
            bricks: vec![brick],
            moment_resolution: 8,
            max_distance: 4.0,
        };
        assert_eq!(layout.probe_count(), usize::MAX);
        assert!(matches!(layout.validate(), Err(BakedGiError::Layout(_))));
    }

    /// Heights outside 1–32 m and a brick coarser than 1–2 m still load. The overview states
    /// the format's defaults, not a reject range.
    #[test]
    fn custom_terrain_levels_and_brick_spacing_load() {
        let mut layout = VolumeLayout {
            terrain: Some(TerrainLayer {
                origin: [0.0, 0.0],
                spacing: [0.5, 0.5],
                counts: [1, 1],
                levels: vec![0.1, 64.0],
                ground: vec![0.0],
            }),
            bricks: vec![DetailBrick {
                origin: [0.0; 3],
                spacing: [8.0, 8.0, 8.0],
                counts: [1, 1, 1],
                first_probe: 0,
                flags: 0,
            }],
            moment_resolution: 4,
            max_distance: 4.0,
        };
        layout.assign_brick_offsets();
        layout
            .validate()
            .expect("custom levels and brick spacing validate");
        let probes = vec![probe(0), probe(1), probe(2)];
        let volume = BakedGiVolume::from_probes(&layout, BakeLights::default(), BakeInfo::default(), &probes)
            .expect("from_probes");
        let terrain = volume.terrain.as_ref().expect("terrain");
        assert_eq!(terrain.levels, vec![0.1, 64.0]);
        assert_eq!(terrain.spacing, [0.5, 0.5]);
        assert_eq!(volume.bricks[0].spacing, [8.0, 8.0, 8.0]);
    }

    fn terrain() -> TerrainLayer {
        TerrainLayer {
            origin: [-8.0, -4.0],
            spacing: [4.0, 4.0],
            counts: [5, 3],
            levels: vec![1.0, 2.0, 4.0, 8.0],
            ground: (0..15).map(|k| k as f32 * 0.5).collect(),
        }
    }

    fn layout() -> VolumeLayout {
        let mut l = VolumeLayout {
            terrain: Some(terrain()),
            bricks: vec![DetailBrick {
                origin: [0.0, 1.0, 0.0],
                spacing: [1.0; 3],
                counts: [2, 3, 2],
                first_probe: 0,
                flags: 0,
            }],
            moment_resolution: 4,
            max_distance: 20.0,
        };
        l.assign_brick_offsets();
        l
    }

    fn probe(seed: usize) -> ProbeData {
        let s = seed as f32;
        let mut sh = [[[0.0f32; 4]; 3]; GI_COMPONENTS];
        for (c, comp) in sh.iter_mut().enumerate() {
            for (ch, coeffs) in comp.iter_mut().enumerate() {
                let l0 = 1.0 + s * 0.37 + c as f32 * 3.0 + ch as f32;
                *coeffs = [l0, 0.3 * l0, -0.9 * l0, 0.05 * l0];
            }
        }
        ProbeData {
            sh,
            moments: (0..16).map(|t| [t as f32, (t * t) as f32 + 1.0]).collect(),
            meta: ProbeMeta {
                offset: [0.25, -0.5, 0.125],
                flags: ProbeFlags::VALID | ProbeFlags::RELOCATED,
            },
        }
    }

    #[test]
    fn f16_round_trips_and_rounds() {
        for v in [0.0f32, 1.0, -2.5, 0.333_333, 65504.0, 6.1e-5, 1e-7, -0.125] {
            let back = f16_to_f32(f32_to_f16(v));
            assert!((back - v).abs() <= v.abs() * 1e-3 + 6e-8, "{v} -> {back}");
        }
        assert_eq!(f32_to_f16(1e6), 0x7c00);
        assert_eq!(f16_to_f32(0x3c00), 1.0);
    }

    #[test]
    fn rgb9e5_round_trips_within_its_precision() {
        for c in [
            [1.0, 0.5, 0.25],
            [1000.0, 3.0, 0.0],
            [0.001, 0.002, 0.0005],
            [65000.0, 1.0, 2.0],
        ] {
            let d = from_rgb9e5(rgb9e5(c));
            let maxc = c.iter().copied().fold(0.0f32, f32::max);
            for k in 0..3 {
                assert!((d[k] - c[k]).abs() <= maxc / 256.0, "{c:?} -> {d:?}");
            }
        }
        assert_eq!(rgb9e5([0.0; 3]), 0);
        assert_eq!(from_rgb9e5(rgb9e5([-1.0, f32::NAN, 0.0])), [0.0; 3]);
    }

    #[test]
    fn octahedral_mapping_inverts() {
        for t in 0..64 {
            let d = texel_direction(t, 8);
            let uv = oct_encode(d);
            let back = oct_decode(uv);
            for k in 0..3 {
                assert!((back[k] - d[k]).abs() < 1e-5, "{d:?} {back:?}");
            }
            let x = (uv[0] * 8.0) as usize;
            let y = (uv[1] * 8.0) as usize;
            assert_eq!(y * 8 + x, t);
        }
    }

    #[test]
    fn constant_radiance_gives_pi_irradiance_everywhere() {
        // L(ω) = 1: L00 = 4π·Y00, L1 = 0, E(n) = π.
        let l00 = 4.0 * std::f32::consts::PI * SH_Y00;
        let sh = [[l00, 0.0, 0.0, 0.0]; 3];
        for n in [[0.0, 1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, -1.0]] {
            for e in irradiance(&sh, n) {
                assert!((e - std::f32::consts::PI).abs() < 1e-4, "{e}");
            }
        }
    }

    #[test]
    fn layout_indices_and_neighbors() {
        let l = layout();
        let t = l.terrain.as_ref().expect("terrain");
        assert_eq!(t.probe_count(), 60);
        assert_eq!(l.probe_count(), 72);
        assert_eq!(l.bricks[0].first_probe, 60);
        for idx in 0..t.probe_count() {
            let (i, j, lv) = t.coords(idx);
            assert_eq!(t.index(i, j, lv), idx);
        }
        let p = t.nominal_position(2, 1, 3);
        assert_eq!(p, [0.0, 3.5 + 8.0, 0.0]);
        // An interior terrain probe has 26 neighbors, a corner 7.
        assert_eq!(l.neighbors(t.index(2, 1, 1)).len(), 26);
        assert_eq!(l.neighbors(t.index(0, 0, 0)).len(), 7);
        let corner = l.bricks[0].index(0, 0, 0);
        assert_eq!(l.neighbors(corner).len(), 7);
        assert!(l.neighbors(corner).iter().all(|(n, _)| *n >= 60));
        assert_eq!(l.nominal_positions().len(), 72);
        assert_eq!(l.half_cells()[t.index(0, 0, 0)], [2.0, 0.5, 2.0]);
    }

    #[test]
    fn volume_round_trips_through_bytes() {
        let l = layout();
        let probes: Vec<ProbeData> = (0..l.probe_count()).map(probe).collect();
        let lights = BakeLights {
            sun_illuminance: 100_000.0,
            sky_scale: 1.0,
            ..BakeLights::default()
        };
        let info = BakeInfo {
            seed: 9,
            rays_per_probe: 128,
            samples: 4,
            max_bounces: 3,
            scene_hash: 0xdead_beef,
            filtered: true,
        };
        let v = BakedGiVolume::from_probes(&l, lights, info, &probes).expect("encode");
        let bytes = v.to_bytes().expect("bytes");
        assert_eq!(&bytes[0..4], b"FGIV");
        let back = BakedGiVolume::from_bytes(&bytes).expect("decode");
        assert_eq!(back, v);
        assert_eq!(back.layout(), l);
        for (i, p) in probes.iter().enumerate() {
            let meta = back.probe_meta(i);
            assert_eq!(meta.flags, p.meta.flags);
            assert_eq!(meta.offset, p.meta.offset);
            for c in GiComponent::ALL {
                let d = back.probe_sh(i, c);
                let src = &p.sh[c as usize];
                for ch in 0..3 {
                    let l0 = src[ch][0];
                    assert!((d[ch][0] - l0).abs() <= l0 * 4e-3, "{c:?} {d:?} {src:?}");
                    for m in 1..4 {
                        assert!((d[ch][m] - src[ch][m]).abs() <= l0 * 0.02, "{c:?} {d:?} {src:?}");
                    }
                }
            }
            for (a, b) in back.probe_moments(i).iter().zip(&p.moments) {
                assert!(
                    (a[0] - b[0]).abs() < 1e-3 && (a[1] - b[1]).abs() < 0.02,
                    "{a:?} {b:?}"
                );
            }
        }
        let n = back.probe_position(0);
        let nominal = l.nominal_positions()[0];
        assert!((n[0] - nominal[0] - 0.25).abs() < 1e-6);
    }

    #[test]
    fn corrupt_or_foreign_files_are_rejected() {
        let l = layout();
        let probes: Vec<ProbeData> = (0..l.probe_count()).map(probe).collect();
        let v = BakedGiVolume::from_probes(&l, BakeLights::default(), BakeInfo::default(), &probes)
            .expect("encode");
        let bytes = v.to_bytes().expect("bytes");
        assert!(matches!(
            BakedGiVolume::from_bytes(b"FCKD...."),
            Err(BakedGiError::BadMagic)
        ));
        // Version 5 is current (ADR-3237) and 1 to 4 still read (`version_1_files_read_with_flags_0`,
        // `version_2_files_read_without_an_emitter_list`, `baked_gi_split`'s
        // `version_3_files_read_unsplit_and_migrate` and `version_4_parts_read_uncropped_and_migrate`);
        // a later one is refused, as is 0.
        let mut wrong = bytes.clone();
        wrong[4] = 6;
        assert!(matches!(
            BakedGiVolume::from_bytes(&wrong),
            Err(BakedGiError::UnsupportedVersion { found: 6, .. })
        ));
        wrong[4] = 0;
        assert!(matches!(
            BakedGiVolume::from_bytes(&wrong),
            Err(BakedGiError::UnsupportedVersion { found: 0, .. })
        ));
        let mut flipped = bytes.clone();
        let last = flipped.len() - 40;
        flipped[last] ^= 0x55;
        assert!(BakedGiVolume::from_bytes(&flipped).is_err());
        assert!(BakedGiVolume::from_bytes(&bytes[..bytes.len() / 2]).is_err());
    }

    #[test]
    fn mismatched_probe_data_is_a_layout_error() {
        let l = layout();
        let probes: Vec<ProbeData> = (0..3).map(probe).collect();
        assert!(matches!(
            BakedGiVolume::from_probes(&l, BakeLights::default(), BakeInfo::default(), &probes),
            Err(BakedGiError::Layout(_))
        ));
        let mut bad = l.clone();
        if let Some(t) = &mut bad.terrain {
            t.levels = vec![2.0, 1.0];
        }
        assert!(bad.validate().is_err());
    }

    #[test]
    fn non_finite_spacing_or_ground_fails_validation() {
        let mut l = layout();
        l.bricks[0].spacing = [f32::INFINITY, 1.0, 1.0];
        assert!(matches!(l.validate(), Err(BakedGiError::Layout(_))));
        let mut l = layout();
        if let Some(t) = &mut l.terrain {
            t.spacing[1] = f32::INFINITY;
        }
        assert!(matches!(l.validate(), Err(BakedGiError::Layout(_))));
        let mut l = layout();
        if let Some(t) = &mut l.terrain {
            t.ground[3] = f32::NAN;
        }
        assert!(matches!(l.validate(), Err(BakedGiError::Layout(_))));
    }

    #[test]
    fn neighbors_work_on_axes_longer_than_i32_max() {
        let brick = DetailBrick {
            origin: [0.0; 3],
            spacing: [1.0; 3],
            counts: [2_200_000_000, 1, 1],
            first_probe: 0,
            flags: 0,
        };
        let layout = VolumeLayout {
            terrain: None,
            bricks: vec![brick],
            moment_resolution: 4,
            max_distance: 4.0,
        };
        assert!(layout.validate().is_ok());
        let n = layout.neighbors(5);
        let mut got: Vec<usize> = n.iter().map(|(i, _)| *i).collect();
        got.sort_unstable();
        assert_eq!(got, vec![4, 6]);
    }

    #[test]
    fn a_zero_exposure_header_is_rejected() {
        let mut v = volume();
        v.exposure[0] = 0.0;
        let bytes = v.to_bytes().expect("encode");
        assert!(matches!(
            BakedGiVolume::from_bytes(&bytes),
            Err(BakedGiError::Payload(_))
        ));
    }

    #[test]
    fn chebyshev_passes_the_near_side_and_blocks_far_behind_a_wall() {
        assert_eq!(chebyshev(2.0, 4.0, 1.5), 1.0);
        assert!(chebyshev(2.0, 4.0, 6.0) < 0.01);
        assert!(chebyshev(2.0, 4.5, 2.2) > 0.5);
    }

    fn volume() -> BakedGiVolume {
        let l = layout();
        let probes: Vec<ProbeData> = (0..l.probe_count()).map(probe).collect();
        BakedGiVolume::from_probes(&l, BakeLights::default(), BakeInfo::default(), &probes).expect("encode")
    }

    #[test]
    fn version_1_files_read_with_flags_0() {
        let v = volume();
        let v1 = v.to_bytes_v1().expect("v1 bytes");
        let v2 = v.to_bytes().expect("v2 bytes");
        assert_eq!(u32::from_le_bytes([v1[4], v1[5], v1[6], v1[7]]), 1);
        assert_eq!(u32::from_le_bytes([v2[4], v2[5], v2[6], v2[7]]), BAKED_GI_VERSION);
        let back = BakedGiVolume::from_bytes(&v1).expect("v1 reads");
        assert_eq!(back, v, "a v1 file decodes to the same volume");
        assert!(back.bricks.iter().all(|b| b.flags == 0));
        // The sections the renderer uploads are byte for byte the same in both versions.
        let again = BakedGiVolume::from_bytes(&v2).expect("v2 reads");
        assert_eq!(back.meta_bytes(), again.meta_bytes());
        assert_eq!(back.sh_bytes(), again.sh_bytes());
        assert_eq!(back.moment_bytes(), again.moment_bytes());
        assert_eq!(back, again);
        // The v1 header is the v2 one without the flags words.
        let len = |b: &[u8]| u32::from_le_bytes([b[8], b[9], b[10], b[11]]) as usize;
        assert!(len(&v2) > len(&v1));
    }

    #[test]
    fn brick_flags_round_trip_and_unknown_bits_are_refused() {
        let mut v = volume();
        v.bricks[0].flags = BrickFlags::INTERIOR;
        assert_eq!(v.interior_brick_count(), 1);
        let back = BakedGiVolume::from_bytes(&v.to_bytes().expect("bytes")).expect("decode");
        assert!(back.bricks[0].is_interior());
        assert_eq!(back, v);
        assert!(
            matches!(v.to_bytes_v1(), Err(BakedGiError::Layout(_))),
            "version 1 cannot hold flags"
        );
        let mut layout = v.layout();
        layout.bricks[0].flags = BrickFlags::RESERVED;
        assert!(layout.validate().is_err(), "reserved bit");
        layout.bricks[0].flags = 1 << 7;
        assert!(layout.validate().is_err(), "unknown bit");
    }

    /// The lookup grid is built once per brick table. A later read reuses it. Editing a flag
    /// builds it again; `first_probe` does not. Reading the grid does not change equality.
    #[test]
    fn an_edited_brick_flag_rebuilds_the_lookup_grid() {
        let mut v = volume();
        let before = crate::baked_gi_grid::brick_grid_builds();
        let first = v.brick_grid_word_count();
        assert_eq!(v.brick_grid_word_count(), first);
        assert_eq!(crate::baked_gi_grid::brick_grid_builds(), before + 1);
        let read = v.clone();
        let _ = read.brick_grid_word_count();
        assert_eq!(read, v);
        assert_eq!(crate::baked_gi_grid::brick_grid_builds(), before + 1);
        v.bricks[0].first_probe = v.bricks[0].first_probe.wrapping_add(1);
        assert_eq!(v.brick_grid_word_count(), first);
        assert_eq!(crate::baked_gi_grid::brick_grid_builds(), before + 1);
        v.bricks[0].flags |= BrickFlags::INTERIOR;
        let _ = v.brick_grid_word_count();
        assert_eq!(crate::baked_gi_grid::brick_grid_builds(), before + 2);
        let _ = v.brick_grid_word_count();
        assert_eq!(crate::baked_gi_grid::brick_grid_builds(), before + 2);
    }

    /// Hand-run timing of a rebuilt grid against a cached read. Nothing here asserts on the clock.
    #[test]
    #[ignore = "hand-run timing for the brick-grid cache"]
    fn brick_grid_cache_timing() {
        let mut layout = VolumeLayout {
            terrain: None,
            bricks: (0..48u16)
                .map(|i| DetailBrick {
                    origin: [f32::from(i) * 4.0, 0.0, 0.0],
                    spacing: [1.0; 3],
                    counts: [2, 2, 2],
                    first_probe: 0,
                    flags: 0,
                })
                .collect(),
            moment_resolution: 4,
            max_distance: 8.0,
        };
        layout.assign_brick_offsets();
        let probes: Vec<ProbeData> = (0..layout.probe_count()).map(probe).collect();
        let volume = BakedGiVolume::from_probes(&layout, BakeLights::default(), BakeInfo::default(), &probes)
            .expect("volume");
        let rounds = 200u32;
        let _ = volume.brick_grid_word_count();
        let start = std::time::Instant::now();
        let mut built_words = 0usize;
        for _ in 0..rounds {
            built_words += crate::baked_gi_grid::BrickGrid::build(&volume.bricks).word_count();
        }
        let built = start.elapsed();
        let start = std::time::Instant::now();
        let mut cached_words = 0usize;
        for _ in 0..rounds {
            cached_words += volume.brick_grid_word_count();
        }
        let cached = start.elapsed();
        eprintln!(
            "brick_grid_cache_timing bricks=48 rounds={rounds} rebuild={built:?} words={built_words} cached={cached:?} words={cached_words}"
        );
    }

    #[test]
    fn interior_boxes_flag_the_bricks_whose_centre_they_hold() {
        let mut l = layout();
        // The fixture brick spans (0, 1, 0)..(1, 3, 1): centre (0.5, 2, 0.5).
        assert_eq!(l.bricks[0].centre(), [0.5, 2.0, 0.5]);
        assert_eq!(l.flag_interior_bricks(&[([2.0, 0.0, 0.0], [4.0, 4.0, 4.0])]), 0);
        assert!(!l.bricks[0].is_interior());
        assert_eq!(l.flag_interior_bricks(&[([0.0, 0.0, 0.0], [1.0, 4.0, 1.0])]), 1);
        assert!(l.bricks[0].is_interior());
        assert!(l.validate().is_ok());
    }

    /// ADR-1386: a version 2 file (every bake before the emitter list) reads with the list not
    /// recorded, keeps its sections byte for byte, and writes back as version 3 without
    /// inventing a list; a version 3 file round-trips the list.
    #[test]
    fn version_2_files_read_without_an_emitter_list() {
        let v = volume();
        let v2 = v.to_bytes_v2().expect("v2 bytes");
        assert_eq!(u32::from_le_bytes([v2[4], v2[5], v2[6], v2[7]]), 2);
        let old = BakedGiVolume::from_bytes(&v2).expect("v2 reads");
        assert_eq!(old.emitters, None, "a v2 bake did not record its emitters");
        assert_eq!(old, v);
        // Migrated: written in the current version, read back unchanged.
        let v3 = old.to_bytes().expect("v3 bytes");
        assert_eq!(u32::from_le_bytes([v3[4], v3[5], v3[6], v3[7]]), BAKED_GI_VERSION);
        let migrated = BakedGiVolume::from_bytes(&v3).expect("v3 reads");
        assert_eq!(migrated, old);
        assert_eq!(migrated.meta_bytes(), old.meta_bytes());
        assert_eq!(migrated.sh_bytes(), old.sh_bytes());
        assert_eq!(migrated.moment_bytes(), old.moment_bytes());
        // A recorded list (also an empty one: "this bake traced no emitter") round-trips.
        let mut baked = v.clone();
        baked.emitters = Some(Vec::new());
        let back = BakedGiVolume::from_bytes(&baked.to_bytes().expect("bytes")).expect("decode");
        assert_eq!(back.emitters, Some(Vec::new()));
        baked.emitters = Some(vec![
            BakedEmitter {
                min: [-0.5, 2.9, -0.5],
                max: [0.5, 3.0, 0.5],
            },
            BakedEmitter {
                min: [4.0, 0.0, 4.0],
                max: [4.0, 1.0, 5.0],
            },
        ]);
        let back = BakedGiVolume::from_bytes(&baked.to_bytes().expect("bytes")).expect("decode");
        assert_eq!(back, baked);
        // The older versions cannot hold the list and refuse rather than drop it.
        assert!(matches!(baked.to_bytes_v2(), Err(BakedGiError::Layout(_))));
        assert!(matches!(baked.to_bytes_v1(), Err(BakedGiError::Layout(_))));
    }

    #[test]
    fn a_baked_emitter_with_bad_bounds_is_refused() {
        let mut v = volume();
        v.emitters = Some(vec![BakedEmitter {
            min: [1.0, 0.0, 0.0],
            max: [0.0, 1.0, 1.0],
        }]);
        let bytes = v.to_bytes().expect("bytes");
        assert!(matches!(
            BakedGiVolume::from_bytes(&bytes),
            Err(BakedGiError::Payload(_))
        ));
        v.emitters = Some(vec![BakedEmitter {
            min: [0.0, f32::NAN, 0.0],
            max: [1.0, 1.0, 1.0],
        }]);
        let bytes = v.to_bytes().expect("bytes");
        assert!(matches!(
            BakedGiVolume::from_bytes(&bytes),
            Err(BakedGiError::Payload(_))
        ));
    }

    #[test]
    fn baked_emitters_match_the_same_bounds_within_the_slack_only() {
        assert_eq!(BakedEmitter::from_points([]), None);
        assert_eq!(BakedEmitter::from_points([[0.0, f32::INFINITY, 0.0]]), None);
        let panel = BakedEmitter::from_points([[-0.5, 3.0, -0.25], [0.5, 2.98, 0.25], [0.0, 3.0, 0.0]])
            .expect("bounds");
        assert_eq!(panel.min, [-0.5, 2.98, -0.25]);
        assert_eq!(panel.max, [0.5, 3.0, 0.25]);
        // The diagonal is ~1.12 m: slack 2 cm + 2 % of it.
        let slack = panel.slack();
        assert!((slack - (0.02 + 0.02 * (1.0f32 + 0.0004 + 0.25).sqrt())).abs() < 1e-6);
        assert!(panel.matches(panel.min, panel.max));
        // Float noise of another transform, and a LOD a centimetre smaller: the same emitter.
        assert!(panel.matches([-0.4999, 2.9801, -0.25], [0.5001, 3.0, 0.2499]));
        assert!(panel.matches([-0.49, 2.98, -0.24], [0.49, 3.0, 0.24]));
        // Moved 10 cm, or the next panel along a row: another emitter.
        assert!(!panel.matches([-0.4, 2.98, -0.25], [0.6, 3.0, 0.25]));
        assert!(!panel.matches([0.5, 2.98, -0.25], [1.5, 3.0, 0.25]));
        // Same centre, other size (a new fitting in the old socket): another emitter.
        assert!(!panel.matches([-0.25, 2.98, -0.25], [0.25, 3.0, 0.25]));
        // The object box under a transform: turned 90° about Y, moved to (3, 1, 0).
        let turned = [
            [0.0, 0.0, -1.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [1.0, 0.0, 0.0, 0.0],
            [3.0, 1.0, 0.0, 1.0],
        ];
        let placed =
            BakedEmitter::from_object_bounds([-0.4, 0.0, -0.2], [0.4, 0.1, 0.2], &turned).expect("bounds");
        assert_eq!(placed.min, [2.8, 1.0, -0.4]);
        assert_eq!(placed.max, [3.2, 1.1, 0.4]);
        let mut bad = turned;
        bad[3][0] = f32::NAN;
        assert_eq!(BakedEmitter::from_object_bounds([0.0; 3], [1.0; 3], &bad), None);
    }

    /// Review baked-light #4: the runtime binding size is the probes' records (8 meta + 4 × 16
    /// SH + 4 per moment texel), the ground heights and 48 bytes per brick.
    #[test]
    fn runtime_buffer_bytes_counts_every_section() {
        let mut l = layout();
        // 72 probes × (8 + 64 + 16 × 4) + 15 ground heights × 4 + 1 brick × 48 + the brick grid
        // (ADR-1835: 8 header words, 2 words per cell, one list entry per binned brick).
        let grid = crate::baked_gi_grid::BrickGrid::build(&l.bricks).word_count() as u64 * 4;
        assert!(grid > 0);
        assert_eq!(l.runtime_buffer_bytes(), 72 * 136 + 15 * 4 + 48 + grid);
        l.moment_resolution = 8;
        assert_eq!(l.runtime_buffer_bytes(), 72 * (8 + 64 + 256) + 15 * 4 + 48 + grid);
        let empty = VolumeLayout {
            terrain: None,
            bricks: Vec::new(),
            moment_resolution: 4,
            max_distance: 1.0,
        };
        assert_eq!(empty.runtime_buffer_bytes(), 16, "the renderer's minimum buffer");
        let v = volume();
        let sections = v.meta_bytes().len() + v.sh_bytes().len() + v.moment_bytes().len();
        assert_eq!(
            v.layout().runtime_buffer_bytes(),
            sections as u64
                + 15 * 4
                + RUNTIME_BRICK_BYTES
                + crate::baked_gi_grid::BrickGrid::build(&v.bricks).word_count() as u64 * 4
        );
    }

    #[test]
    fn wall_codes_hold_a_position_per_axis_and_round_trip() {
        let fields = [ProbeFlags::WALL_X, ProbeFlags::WALL_Y, ProbeFlags::WALL_Z];
        for (axis, field) in fields.iter().enumerate() {
            for t in [0.0, 0.01, 0.3, 0.5, 0.97, 1.0, 2.0] {
                let bits = ProbeFlags::wall(axis, t);
                assert_eq!(bits & field, bits, "axis {axis}");
                let at = ProbeFlags::wall_at(bits, axis).expect("a wall");
                assert!((at - t.clamp(0.0, 1.0)).abs() <= 0.5 / 15.0 + 1e-6, "{t} → {at}");
                for other in (0..3).filter(|&o| o != axis) {
                    assert_eq!(ProbeFlags::wall_at(bits, other), None);
                }
            }
            assert_eq!(ProbeFlags::wall_at(ProbeFlags::VALID, axis), None);
        }
        assert_eq!(ProbeFlags::WALLS & (ProbeFlags::VALID | ProbeFlags::RELOCATED), 0);
        assert_eq!(
            ProbeFlags::WALLS & (ProbeFlags::UNDER_TERRAIN | ProbeFlags::INSIDE_GEOMETRY),
            0
        );
        let l = layout();
        let probes: Vec<ProbeData> = (0..l.probe_count())
            .map(|i| {
                let mut p = probe(i);
                if i.is_multiple_of(5) {
                    p.meta.flags |= ProbeFlags::wall(2, 0.25);
                }
                p
            })
            .collect();
        let v = BakedGiVolume::from_probes(&l, BakeLights::default(), BakeInfo::default(), &probes)
            .expect("encode");
        let walled = (0..l.probe_count()).filter(|i| i.is_multiple_of(5)).count();
        assert_eq!(v.wall_probe_count(), walled);
        assert_eq!(volume().wall_probe_count(), 0);
        let back = BakedGiVolume::from_bytes(&v.to_bytes().expect("bytes")).expect("decode");
        let m = back.probe_meta(5);
        assert!(m.is_valid());
        assert_eq!(m.flags & ProbeFlags::WALLS, ProbeFlags::wall(2, 0.25));
    }
}

#[cfg(test)]
mod section_mutations {
    use super::*;

    const FILE_BYTES: usize = 2304;
    const PAYLOAD_START: usize = 1024;

    /// Public writer output supplies every record; only the existing version serializer
    /// writes structured mutations. Fixed small gaps leave room for varint growth.
    fn fixture() -> (VolumeHeader, Vec<u8>) {
        let mut layout = VolumeLayout {
            terrain: Some(TerrainLayer {
                origin: [0.0; 2],
                spacing: [1.0; 2],
                counts: [1; 2],
                levels: vec![1.0],
                ground: vec![0.5],
            }),
            bricks: vec![DetailBrick {
                origin: [0.0; 3],
                spacing: [1.0; 3],
                counts: [1; 3],
                first_probe: 0,
                flags: 0,
            }],
            moment_resolution: 2,
            max_distance: 8.0,
        };
        layout.assign_brick_offsets();
        let probe = ProbeData {
            sh: std::array::from_fn(|c| [[(c + 1) as f32, 0.1, -0.2, 0.0]; 3]),
            moments: vec![[1.0, 2.0]; 4],
            meta: ProbeMeta {
                offset: [0.0; 3],
                flags: ProbeFlags::VALID,
            },
        };
        let bytes = BakedGiVolume::from_probes(
            &layout,
            BakeLights::default(),
            BakeInfo::default(),
            &[probe.clone(), probe],
        )
        .expect("public writer volume")
        .to_bytes()
        .expect("public writer bytes");
        let len = u32::from_le_bytes(bytes[8..12].try_into().expect("header length")) as usize;
        let mut header: VolumeHeader = postcard::from_bytes(&bytes[12..12 + len]).expect("writer header");
        let mut payload = vec![0; FILE_BYTES];
        for (section, offset) in header.sections.iter_mut().zip([1024, 1152, 1792, 2048]) {
            let start = section.offset as usize;
            let len = section.len as usize;
            payload[offset..offset + len].copy_from_slice(&bytes[start..start + len]);
            section.offset = offset as u64;
        }
        (header, payload)
    }

    fn write_header(bytes: &mut [u8], header: &VolumeHeader, version: u32) {
        let encoded = encode_header(header, version).expect("existing header serializer");
        assert!(12 + encoded.len() < PAYLOAD_START);
        bytes[..4].copy_from_slice(&BAKED_GI_MAGIC);
        bytes[4..8].copy_from_slice(&version.to_le_bytes());
        bytes[8..12].copy_from_slice(&(encoded.len() as u32).to_le_bytes());
        bytes[12..12 + encoded.len()].copy_from_slice(&encoded);
    }

    fn file(mut header: VolumeHeader, payload: &[u8], version: u32, rehash: bool) -> Vec<u8> {
        assert_eq!(payload.len(), FILE_BYTES);
        let mut bytes = payload.to_vec();
        write_header(&mut bytes, &header, version);
        if rehash {
            let mut hasher = blake3::Hasher::new();
            for s in header.sections {
                let start = usize::try_from(s.offset).expect("bounded rehash offset");
                let len = usize::try_from(s.len).expect("bounded rehash length");
                hasher.update(&bytes[start..start + len]);
            }
            header.payload_hash = *hasher.finalize().as_bytes();
            write_header(&mut bytes, &header, version);
        }
        bytes
    }

    fn payload_error(bytes: &[u8], context: &str, reason: &str) {
        match BakedGiVolume::from_bytes(bytes) {
            Err(BakedGiError::Payload(message)) => assert!(
                message.contains(reason),
                "{context}: expected {reason:?}, got {message:?}"
            ),
            other => panic!("{context}: expected Payload({reason}), got {other:?}"),
        }
    }

    #[test]
    fn section_range_boundary_matrix() {
        let (header, payload) = fixture();
        let mut checked = 0;
        for version in BAKED_GI_MIN_VERSION..=BAKED_GI_VERSION {
            BakedGiVolume::from_bytes(&file(header.clone(), &payload, version, true)).expect("baseline");
            for k in 0..4 {
                let original = header.sections[k];
                for (label, offset, len) in [
                    ("offset max", u64::MAX, original.len),
                    ("length max", original.offset, u64::MAX),
                    ("end overflow", u64::MAX - 15, 32),
                    ("end one past EOF", (FILE_BYTES - 16) as u64, 17),
                    ("nonempty at EOF", FILE_BYTES as u64, 1),
                    ("empty past EOF", FILE_BYTES as u64 + 16, 0),
                ] {
                    let mut h = header.clone();
                    h.sections[k] = SectionRef { offset, len };
                    payload_error(
                        &file(h, &payload, version, false),
                        &format!("v{version} section {k} {label}"),
                        "runs past the end",
                    );
                    checked += 1;
                }
            }
        }
        assert_eq!(checked, 5 * 4 * 6);
    }

    #[test]
    fn section_alignment_and_header_aliases_are_rejected_with_valid_hashes() {
        let (header, payload) = fixture();
        let mut checked = 0;
        for version in BAKED_GI_MIN_VERSION..=BAKED_GI_VERSION {
            for k in 0..4 {
                for delta in 1..16 {
                    let mut h = header.clone();
                    h.sections[k].offset += delta;
                    payload_error(
                        &file(h, &payload, version, true),
                        &format!("v{version} section {k} misalignment {delta}"),
                        "aligned",
                    );
                    checked += 1;
                }
                let mut h = header.clone();
                h.sections[k].offset = 0;
                // Keep the original record count. These tiny sections reference only header
                // bytes before payload_hash, so rehashing does not hash its own digest.
                payload_error(
                    &file(h, &payload, version, true),
                    &format!("v{version} section {k} header alias"),
                    "header",
                );
                checked += 1;
            }
        }
        assert_eq!(checked, 5 * 4 * 16);
    }

    #[test]
    fn section_pair_aliases_and_partial_overlaps_are_rejected_with_valid_hashes() {
        let (header, payload) = fixture();
        let mut checked = 0;
        for version in BAKED_GI_MIN_VERSION..=BAKED_GI_VERSION {
            for a in 0..4 {
                for b in a + 1..4 {
                    for delta in [0, 16] {
                        let mut h = header.clone();
                        // Keep each original record stride; the hash matches even when two
                        // differently sized sections name some of the same bytes.
                        let (short, long) = if h.sections[a].len < h.sections[b].len {
                            (a, b)
                        } else {
                            (b, a)
                        };
                        if delta != 0 && h.sections[long].len <= delta {
                            continue; // No partial overlap of these sizes can be 16-byte aligned.
                        }
                        h.sections[short].offset = h.sections[long].offset + delta;
                        payload_error(
                            &file(h, &payload, version, true),
                            &format!("v{version} pair {a}/{b} delta {delta}"),
                            "overlap",
                        );
                        checked += 1;
                    }
                }
            }
        }
        assert_eq!(checked, 5 * 11);
    }

    #[test]
    fn section_record_stride_matrix_includes_ground_remainders() {
        let (header, payload) = fixture();
        let mut checked = 0;
        for version in BAKED_GI_MIN_VERSION..=BAKED_GI_VERSION {
            for k in 0..4 {
                let original = header.sections[k].len;
                for len in [
                    original + 1,
                    original + 2,
                    original + 3,
                    0,
                    original - 1,
                    original * 2,
                ] {
                    let mut h = header.clone();
                    h.sections[k].len = len;
                    // This reaches record-size validation, rather than being stopped by
                    // the hash check. A trailing ground byte used to be silently ignored.
                    let result = BakedGiVolume::from_bytes(&file(h, &payload, version, true));
                    assert!(
                        matches!(result, Err(BakedGiError::Payload(_))),
                        "v{version} section {k} length {len}: {result:?}"
                    );
                    checked += 1;
                }
            }
        }
        assert_eq!(checked, 5 * 4 * 6);
    }

    #[test]
    fn structured_counts_visibility_encodings_and_reserved_bits() {
        let (header, payload) = fixture();
        for version in BAKED_GI_MIN_VERSION..=BAKED_GI_VERSION {
            for count in [0, 1, 3, u32::MAX] {
                let mut h = header.clone();
                h.probe_count = count;
                payload_error(&file(h, &payload, version, true), "probe count", "section sizes");
            }
            for resolution in [0, 33, u32::MAX] {
                let mut h = header.clone();
                h.moment_resolution = resolution;
                let result = BakedGiVolume::from_bytes(&file(h, &payload, version, true));
                assert!(matches!(result, Err(BakedGiError::Layout(_))), "{result:?}");
            }
            for resolution in [1, 3, 32] {
                let mut h = header.clone();
                h.moment_resolution = resolution;
                payload_error(
                    &file(h, &payload, version, true),
                    "moment stride",
                    "section sizes",
                );
            }
            for distance in [0.0, -1.0, f32::INFINITY, f32::NAN] {
                let mut h = header.clone();
                h.max_distance = distance;
                let result = BakedGiVolume::from_bytes(&file(h, &payload, version, true));
                assert!(matches!(result, Err(BakedGiError::Layout(_))), "{result:?}");
            }
            for encoding in [0, 2, u32::MAX] {
                for sh in [true, false] {
                    let mut h = header.clone();
                    if sh {
                        h.sh_encoding = encoding;
                    } else {
                        h.moment_encoding = encoding;
                    }
                    payload_error(&file(h, &payload, version, true), "encoding", "unknown encodings");
                }
            }
            if version >= 2 {
                for bit in 1..32 {
                    let mut h = header.clone();
                    h.bricks[0].flags = BrickFlags::INTERIOR | (1 << bit);
                    let result = BakedGiVolume::from_bytes(&file(h, &payload, version, true));
                    assert!(
                        matches!(result, Err(BakedGiError::Layout(_))),
                        "bit {bit}: {result:?}"
                    );
                }
            }
            let mut h = header.clone();
            h.bricks[0].first_probe = 0;
            let result = BakedGiVolume::from_bytes(&file(h, &payload, version, true));
            assert!(matches!(result, Err(BakedGiError::Layout(_))), "{result:?}");
            let mut h = header.clone();
            h.terrain = None;
            payload_error(
                &file(h, &payload, version, true),
                "orphan ground",
                "without a terrain",
            );
        }
    }

    #[test]
    fn every_section_is_hash_checked_and_sh_padding_is_preserved() {
        let (header, payload) = fixture();
        for version in BAKED_GI_MIN_VERSION..=BAKED_GI_VERSION {
            let valid = file(header.clone(), &payload, version, true);
            let expected = BakedGiVolume::from_bytes(&valid).expect("baseline");
            for (k, section) in header.sections.iter().enumerate() {
                let mut corrupt = valid.clone();
                corrupt[section.offset as usize] ^= 1;
                payload_error(
                    &corrupt,
                    &format!("v{version} section {k} corruption"),
                    "hash mismatch",
                );
            }
            let mut padded = payload.clone();
            let sh = header.sections[1];
            for record in padded[sh.offset as usize..(sh.offset + sh.len) as usize]
                .as_chunks_mut::<SH_RECORD_BYTES>()
                .0
            {
                record[13..].fill(0xa5);
            }
            let loaded = BakedGiVolume::from_bytes(&file(header.clone(), &padded, version, true))
                .expect("SH record padding is not a reserved flag");
            for i in 0..2 {
                for c in GiComponent::ALL {
                    assert_eq!(loaded.probe_sh(i, c), expected.probe_sh(i, c));
                }
            }
            assert_ne!(loaded.sh_bytes(), expected.sh_bytes());
            assert_eq!(
                BakedGiVolume::from_bytes(&loaded.to_bytes().expect("rewrite"))
                    .expect("reload")
                    .sh_bytes(),
                loaded.sh_bytes()
            );
        }
    }

    #[test]
    fn padding_gaps_reordered_sections_and_empty_sections_remain_accepted() {
        let (header, mut payload) = fixture();
        for version in BAKED_GI_MIN_VERSION..=BAKED_GI_VERSION {
            let valid = file(header.clone(), &payload, version, true);
            let expected = BakedGiVolume::from_bytes(&valid).expect("baseline");
            let mut padding = valid.clone();
            padding[PAYLOAD_START - 1] = 0x55;
            padding[FILE_BYTES - 1] = 0xaa;
            assert_eq!(BakedGiVolume::from_bytes(&padding).expect("padding"), expected);
            let mut h = header.clone();
            let old = h.sections[0].offset as usize;
            let new = 2112;
            payload[new..new + 16].copy_from_slice(&valid[old..old + 16]);
            h.sections[0].offset = new as u64;
            assert_eq!(
                BakedGiVolume::from_bytes(&file(h, &payload, version, true))
                    .expect("disjoint out-of-order sections"),
                expected
            );
            let mut empty = header.clone();
            empty.probe_count = 0;
            empty.terrain = None;
            empty.bricks.clear();
            for offset in [0, PAYLOAD_START as u64, FILE_BYTES as u64] {
                empty.sections = [SectionRef { offset, len: 0 }; 4];
                let loaded = BakedGiVolume::from_bytes(&file(empty.clone(), &payload, version, true))
                    .expect("empty sections share an aligned boundary");
                assert_eq!(loaded.probe_count(), 0);
                assert!(
                    loaded.meta_bytes().is_empty()
                        && loaded.sh_bytes().is_empty()
                        && loaded.moment_bytes().is_empty()
                );
            }
        }
    }
}
