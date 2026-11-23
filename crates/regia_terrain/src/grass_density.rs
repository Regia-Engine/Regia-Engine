// SPDX-License-Identifier: MPL-2.0
//! Painted grass density: a brush over a per-map density image and its side file
//! `<map>.grassdensity.bin` (format v1, ADR-2411, `docs/formats/grass-density.md`).
//!
//! The grass mask ([`crate::grass::GrassMask`]) is baked once from the layers, the bands and the
//! density maps a `.grass.ron` names, so an edit made through
//! [`crate::grass::TerrainGrassHandle::scale_density`] lives only as long as the session. A
//! [`GrassDensityPaint`] is the saved form of those edits: one 8-bit multiplier per texel over the
//! whole map (row 0 at −Z, `255` = the baked density, `0` = no grass), painted by
//! [`GrassDensityPaint::stroke`] and read by the grass host as a per-class multiplier
//! ([`GrassDensityPaint::multipliers_for`], fed to `GrassMaskInputs::multipliers`).
//!
//! A stroke is a pure function of the image and the stroke: each texel under the brush is
//! written once, from its value before the stroke, with the brush weight at its nearest point of
//! the path, in a fixed order (a drag painted as pieces is the same pieces in the same order on
//! every machine). Its [`GrassDensityUndo`] holds the bytes of the touched
//! rectangle before and after, so undo and redo restore exactly.

use std::path::{Path, PathBuf};

use regia_app::prelude::*;
use serde::{Deserialize, Serialize};

use crate::grass::{GrassMultipliers, GrassSet, MultiplierImage};
use crate::sampler::TerrainMap;

/// Version of the `<map>.grassdensity.bin` blob.
pub const GRASS_DENSITY_FORMAT_VERSION: u32 = 1;

/// Largest image side, texels (the grass mask's own cap, [`crate::grass::mask::MAX_MASK_SIDE`]).
pub const MAX_GRASS_DENSITY_SIDE: u32 = 4096;

/// Most path points one stroke may carry.
pub const MAX_GRASS_DENSITY_PATH: usize = 4096;

/// Largest brush radius, metres.
pub const MAX_GRASS_DENSITY_RADIUS_M: f32 = 4096.0;

/// Most texel checks one stroke may cost. The count is the axis-aligned texel box of the path,
/// expanded by the brush radius, times the number of path segments (one when the path is a single
/// point), so one call cannot stall the frame for minutes.
pub const MAX_GRASS_DENSITY_WORK: u64 = 1 << 28;

/// Set by the editor when the open level's grass density file exists and cannot be read.
///
/// `terrain.grass_density_stroke` paints nothing while [`Self::refused`] is true. Absent, or
/// `refused: false`, means a stroke may create or edit the image.
#[derive(Resource, Debug, Clone, Copy, Default)]
pub struct GrassDensityFileRefused {
    /// True while that file stays unreadable.
    pub refused: bool,
}

/// The grass density file of a map, next to it and named as [`crate::holes_ron_path`] names the
/// holes file: `maps/switchbacks.level.ron` → `maps/switchbacks.grassdensity.bin`.
#[must_use]
pub fn grass_density_path(map: &Path) -> PathBuf {
    let holes = crate::holes_ron_path(map);
    let name = holes.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    let stem = name.strip_suffix(".holes.ron").unwrap_or(name);
    holes.with_file_name(format!("{stem}.grassdensity.bin"))
}

/// Errors of painted grass density.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum GrassDensityError {
    /// The image side is outside `2..=MAX_GRASS_DENSITY_SIDE`.
    #[error("grass density side {0} is outside 2..={MAX_GRASS_DENSITY_SIDE}")]
    BadSide(u32),
    /// The blob is not a grass density file (postcard failure or a wrong texel count).
    #[error("grass density blob: {0}")]
    Decode(String),
    /// The blob's version is not one this build reads.
    #[error("grass density version {found} (this build reads {GRASS_DENSITY_FORMAT_VERSION})")]
    BadVersion {
        /// The version in the blob.
        found: u32,
    },
    /// A stroke parameter is out of range; names the field.
    #[error("grass density stroke: {0}")]
    BadStroke(String),
}

/// One grass density stroke: a brush of `radius_m` dragged along `path` (world XZ metres),
/// moving every texel under it toward `target` by the brush weight × `strength`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GrassDensityStroke {
    /// Path points, world XZ metres (one point stamps once).
    pub path: Vec<[f32; 2]>,
    /// Brush radius, metres.
    pub radius_m: f32,
    /// Fraction of the radius at full weight, `0..=1` (the rest falls off smoothly).
    pub hardness: f32,
    /// Weight at the brush centre, `0..=1`.
    pub strength: f32,
    /// The density the brush moves toward, `0..=1` of the baked density (0 clears grass, 1
    /// restores it).
    pub target: f32,
}

impl GrassDensityStroke {
    /// Checks the stroke and names the first bad field.
    ///
    /// # Errors
    ///
    /// [`GrassDensityError::BadStroke`].
    pub fn validate(&self) -> Result<(), GrassDensityError> {
        let bad = |m: String| Err(GrassDensityError::BadStroke(m));
        if self.path.is_empty() || self.path.len() > MAX_GRASS_DENSITY_PATH {
            return bad(format!(
                "path has {} points (1..={MAX_GRASS_DENSITY_PATH})",
                self.path.len()
            ));
        }
        if self.path.iter().flatten().any(|v| !v.is_finite()) {
            return bad("path has a non-finite point".into());
        }
        if !(self.radius_m.is_finite() && self.radius_m > 0.0 && self.radius_m <= MAX_GRASS_DENSITY_RADIUS_M)
        {
            return bad(format!(
                "radius {} outside (0, {MAX_GRASS_DENSITY_RADIUS_M}]",
                self.radius_m
            ));
        }
        for (name, v) in [
            ("hardness", self.hardness),
            ("strength", self.strength),
            ("target", self.target),
        ] {
            if !(v.is_finite() && (0.0..=1.0).contains(&v)) {
                return bad(format!("{name} {v} outside 0..=1"));
            }
        }
        Ok(())
    }
}

/// The bytes a stroke changed: the touched rectangle before and after (row-major, `width` wide).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrassDensityUndo {
    /// First column of the rectangle.
    pub x0: u32,
    /// First row of the rectangle.
    pub y0: u32,
    /// Columns of the rectangle.
    pub width: u32,
    /// Rows of the rectangle.
    pub height: u32,
    /// Texels before the stroke.
    pub before: Vec<u8>,
    /// Texels after the stroke.
    pub after: Vec<u8>,
    /// Texels whose value changed.
    pub changed: u64,
}

impl GrassDensityUndo {
    fn write(&self, paint: &mut GrassDensityPaint, bytes: &[u8]) -> bool {
        let side = paint.side as usize;
        if self.x0 as usize + self.width as usize > side
            || self.y0 as usize + self.height as usize > side
            || bytes.len() != self.width as usize * self.height as usize
        {
            return false;
        }
        for (r, row) in bytes.chunks(self.width.max(1) as usize).enumerate() {
            let start = (self.y0 as usize + r) * side + self.x0 as usize;
            paint.texels[start..start + row.len()].copy_from_slice(row);
        }
        paint.generation += 1;
        paint.unsaved = true;
        true
    }

    /// Restores the texels before the stroke. `false` (and nothing written) when the record does
    /// not fit `paint` (another image).
    pub fn undo(&self, paint: &mut GrassDensityPaint) -> bool {
        self.write(paint, &self.before)
    }

    /// Writes the stroke's texels again. `false` when the record does not fit `paint`.
    pub fn redo(&self, paint: &mut GrassDensityPaint) -> bool {
        self.write(paint, &self.after)
    }
}

/// A map's painted grass density (see the module docs). Also a component: the terrain entity
/// that carries one has its grass baked with it by hosts that read
/// [`GrassDensityPaint::multipliers_for`].
#[derive(Component, Clone, Debug, Serialize, Deserialize)]
pub struct GrassDensityPaint {
    /// Blob version.
    version: u32,
    /// Texels per side.
    side: u32,
    /// `side × side` multipliers, row 0 at −Z, `255` = ×1.
    texels: Vec<u8>,
    /// Bumped by every edit (not saved).
    #[serde(skip)]
    generation: u64,
    /// Edited since it was made, loaded or last saved (not saved).
    #[serde(skip)]
    unsaved: bool,
}

/// Images are equal when their version, side and texels are (the edit counters are not part of it).
impl PartialEq for GrassDensityPaint {
    fn eq(&self, other: &Self) -> bool {
        self.version == other.version && self.side == other.side && self.texels == other.texels
    }
}

impl Eq for GrassDensityPaint {}

fn check_side(side: u32) -> Result<(), GrassDensityError> {
    if (2..=MAX_GRASS_DENSITY_SIDE).contains(&side) {
        Ok(())
    } else {
        Err(GrassDensityError::BadSide(side))
    }
}

/// Smooth falloff: 1 up to `hardness` of the radius, then a smoothstep down to 0 at the edge.
fn falloff(t: f32, hardness: f32) -> f32 {
    if t <= hardness {
        return 1.0;
    }
    if t >= 1.0 {
        return 0.0;
    }
    let u = ((t - hardness) / (1.0 - hardness).max(1e-6)).clamp(0.0, 1.0);
    1.0 - u * u * (3.0 - 2.0 * u)
}

/// Squared distance from `p` to the segment `a`–`b`.
fn segment_distance_sq(p: [f32; 2], a: [f32; 2], b: [f32; 2]) -> f32 {
    let d = [b[0] - a[0], b[1] - a[1]];
    let len_sq = d[0] * d[0] + d[1] * d[1];
    let t = if len_sq < 1e-12 {
        0.0
    } else {
        (((p[0] - a[0]) * d[0] + (p[1] - a[1]) * d[1]) / len_sq).clamp(0.0, 1.0)
    };
    let q = [a[0] + d[0] * t - p[0], a[1] + d[1] * t - p[1]];
    q[0] * q[0] + q[1] * q[1]
}

impl GrassDensityPaint {
    /// A blank image (every texel 255: the baked density everywhere).
    ///
    /// # Errors
    ///
    /// [`GrassDensityError::BadSide`].
    pub fn new(side: u32) -> Result<Self, GrassDensityError> {
        check_side(side)?;
        Ok(Self {
            version: GRASS_DENSITY_FORMAT_VERSION,
            side,
            texels: vec![255; side as usize * side as usize],
            generation: 0,
            unsaved: false,
        })
    }

    /// A blank image over `map` with texels `texel_m` metres wide (the grass mask's side rule:
    /// `ceil(world / texel)`, clamped to `2..=MAX_GRASS_DENSITY_SIDE`; a non-positive texel is 1 m).
    ///
    /// # Errors
    ///
    /// [`GrassDensityError::BadSide`] never in practice (the side is clamped).
    pub fn for_map(map: &TerrainMap, texel_m: f32) -> Result<Self, GrassDensityError> {
        let texel = if texel_m.is_finite() && texel_m > 0.0 {
            texel_m
        } else {
            1.0
        };
        let side = ((map.world_width() / texel).ceil() as u32).clamp(2, MAX_GRASS_DENSITY_SIDE);
        Self::new(side)
    }

    /// Texels per side.
    #[must_use]
    pub fn side(&self) -> u32 {
        self.side
    }

    /// The texels, row 0 at −Z.
    #[must_use]
    pub fn texels(&self) -> &[u8] {
        &self.texels
    }

    /// Edit counter (bumped by every stroke, undo and redo; 0 for a fresh or loaded image).
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Whether a stroke, undo or redo changed the image since it was made, loaded or last
    /// [`Self::mark_saved`] (every editor and agent path sets it, so a save finds every edit).
    #[must_use]
    pub fn is_unsaved(&self) -> bool {
        self.unsaved
    }

    /// Marks the image saved.
    pub fn mark_saved(&mut self) {
        self.unsaved = false;
    }

    /// Texels below 255 (where the brush took grass away).
    #[must_use]
    pub fn painted_texels(&self) -> u64 {
        self.texels.iter().filter(|&&t| t < 255).count() as u64
    }

    /// The multiplier at world `(x, z)` on a map `world` metres wide (nearest texel).
    #[must_use]
    pub fn sample(&self, x: f32, z: f32, world: f32) -> f32 {
        self.as_multiplier().sample(x, z, world)
    }

    /// The image as a grass multiplier ([`MultiplierImage`], the same layout).
    #[must_use]
    pub fn as_multiplier(&self) -> MultiplierImage {
        MultiplierImage {
            side: self.side,
            texels: self.texels.clone(),
        }
    }

    /// The per-class multipliers a grass bake reads: this image for every class of `set`,
    /// multiplied into `base`'s image of the class where it has one (biomes, spec 65 § 8), texel
    /// by texel at this image's resolution (`base` sampled nearest, rounded to 8 bits).
    #[must_use]
    pub fn multipliers_for(&self, set: &GrassSet, base: Option<&GrassMultipliers>) -> GrassMultipliers {
        let mut by_class = std::collections::BTreeMap::new();
        let side = self.side as usize;
        for class in &set.classes {
            let image = match base.and_then(|b| b.by_class.get(&class.name)) {
                None => self.as_multiplier(),
                Some(other) => {
                    // Sample `other` at this image's texel centres on a unit map.
                    let mut texels = Vec::with_capacity(self.texels.len());
                    for r in 0..side {
                        for c in 0..side {
                            let x = (c as f32 + 0.5) / side as f32 - 0.5;
                            let z = (r as f32 + 0.5) / side as f32 - 0.5;
                            let m = other.sample(x, z, 1.0) * f32::from(self.texels[r * side + c]);
                            texels.push((m.clamp(0.0, 255.0) + 0.5) as u8);
                        }
                    }
                    MultiplierImage {
                        side: self.side,
                        texels,
                    }
                }
            };
            by_class.insert(class.name.clone(), image);
        }
        if let Some(base) = base {
            for (name, image) in &base.by_class {
                by_class.entry(name.clone()).or_insert_with(|| image.clone());
            }
        }
        GrassMultipliers { by_class }
    }

    /// Applies one stroke over a map `world` metres wide (see the module docs). Returns the
    /// changed rectangle's bytes for undo; an empty record (`changed == 0`, no bytes) when the
    /// brush touched nothing.
    ///
    /// # Errors
    ///
    /// [`GrassDensityError::BadStroke`] for a bad stroke or a non-positive `world`.
    pub fn stroke(
        &mut self,
        stroke: &GrassDensityStroke,
        world: f32,
    ) -> Result<GrassDensityUndo, GrassDensityError> {
        let _span =
            tracing::info_span!(target: "regia_terrain", "grass_density.stroke", points = stroke.path.len())
                .entered();
        stroke.validate()?;
        if !(world.is_finite() && world > 0.0) {
            return Err(GrassDensityError::BadStroke(format!(
                "map width {world} is not positive"
            )));
        }
        let side = self.side as usize;
        let texel = world / self.side as f32;
        let half = world * 0.5;
        let r = stroke.radius_m;
        let (mut lo, mut hi) = ([f32::MAX; 2], [f32::MIN; 2]);
        for p in &stroke.path {
            lo = [lo[0].min(p[0] - r), lo[1].min(p[1] - r)];
            hi = [hi[0].max(p[0] + r), hi[1].max(p[1] + r)];
        }
        let to_texel = |v: f32| ((v + half) / texel - 0.5).floor();
        let clamp = |v: f32| (v.max(0.0) as usize).min(side - 1);
        let (c0, c1) = (to_texel(lo[0]), to_texel(hi[0]) + 1.0);
        let (r0, r1) = (to_texel(lo[1]), to_texel(hi[1]) + 1.0);
        if c1 < 0.0 || r1 < 0.0 || c0 > (side - 1) as f32 || r0 > (side - 1) as f32 {
            return Ok(self.empty_undo());
        }
        let (c0, c1, r0, r1) = (clamp(c0), clamp(c1), clamp(r0), clamp(r1));
        let width = c1 - c0 + 1;
        let height = r1 - r0 + 1;
        let segments = stroke.path.len().saturating_sub(1).max(1) as u64;
        let work = (width as u64) * (height as u64) * segments;
        if work > MAX_GRASS_DENSITY_WORK {
            return Err(GrassDensityError::BadStroke(format!(
                "the stroke would cost {work} texel checks (limit {MAX_GRASS_DENSITY_WORK}); split the path or use a smaller radius"
            )));
        }
        let target = stroke.target * 255.0;
        let mut before = Vec::with_capacity(width * height);
        let mut after = Vec::with_capacity(width * height);
        let mut changed = 0u64;
        let r_sq = r * r;
        for row in r0..=r1 {
            let z = (row as f32 + 0.5) * texel - half;
            for col in c0..=c1 {
                let x = (col as f32 + 0.5) * texel - half;
                let p = [x, z];
                let mut d_sq = f32::MAX;
                if stroke.path.len() == 1 {
                    d_sq = segment_distance_sq(p, stroke.path[0], stroke.path[0]);
                } else {
                    for pair in stroke.path.windows(2) {
                        d_sq = d_sq.min(segment_distance_sq(p, pair[0], pair[1]));
                    }
                }
                let i = row * side + col;
                let old = self.texels[i];
                before.push(old);
                let new = if d_sq <= r_sq {
                    let alpha = falloff(d_sq.sqrt() / r, stroke.hardness) * stroke.strength;
                    let v = f32::from(old) + (target - f32::from(old)) * alpha;
                    (v.clamp(0.0, 255.0) + 0.5) as u8
                } else {
                    old
                };
                if new != old {
                    changed += 1;
                    self.texels[i] = new;
                }
                after.push(new);
            }
        }
        if changed == 0 {
            return Ok(self.empty_undo());
        }
        self.generation += 1;
        self.unsaved = true;
        Ok(GrassDensityUndo {
            x0: c0 as u32,
            y0: r0 as u32,
            width: width as u32,
            height: height as u32,
            before,
            after,
            changed,
        })
    }

    fn empty_undo(&self) -> GrassDensityUndo {
        GrassDensityUndo {
            x0: 0,
            y0: 0,
            width: 0,
            height: 0,
            before: Vec::new(),
            after: Vec::new(),
            changed: 0,
        }
    }

    /// FNV-1a 64 of the side and the texels: equal images give equal digests on every machine.
    #[must_use]
    pub fn digest(&self) -> u64 {
        let mut h = fnv_start();
        h = fnv(h, &self.side.to_le_bytes());
        fnv(h, &self.texels)
    }

    /// Serialises the image (postcard, versioned; `docs/formats/grass-density.md`).
    ///
    /// # Errors
    ///
    /// [`GrassDensityError::Decode`] when postcard fails.
    pub fn to_bytes(&self) -> Result<Vec<u8>, GrassDensityError> {
        postcard::to_allocvec(self).map_err(|e| GrassDensityError::Decode(e.to_string()))
    }

    /// Reads an image written by [`Self::to_bytes`]. The version is read before the body, so a
    /// newer file is named as such.
    ///
    /// # Errors
    ///
    /// [`GrassDensityError`] for a bad blob, version, side or texel count.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, GrassDensityError> {
        let (version, _) =
            postcard::take_from_bytes::<u32>(bytes).map_err(|e| GrassDensityError::Decode(e.to_string()))?;
        if version != GRASS_DENSITY_FORMAT_VERSION {
            return Err(GrassDensityError::BadVersion { found: version });
        }
        // The side comes before the texels: refuse a hostile side before postcard allocates.
        let (_, rest) =
            postcard::take_from_bytes::<u32>(bytes).map_err(|e| GrassDensityError::Decode(e.to_string()))?;
        let (side, _) =
            postcard::take_from_bytes::<u32>(rest).map_err(|e| GrassDensityError::Decode(e.to_string()))?;
        check_side(side)?;
        let paint: Self =
            postcard::from_bytes(bytes).map_err(|e| GrassDensityError::Decode(e.to_string()))?;
        if paint.texels.len() != side as usize * side as usize {
            return Err(GrassDensityError::Decode(format!(
                "{} texels for side {side}",
                paint.texels.len()
            )));
        }
        Ok(paint)
    }
}

pub(crate) fn fnv_start() -> u64 {
    0xcbf2_9ce4_8422_2325
}

pub(crate) fn fnv(mut h: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// The serde field list of the `<map>.grassdensity.bin` blob (hashed by `cargo xtask check-formats`).
#[must_use]
pub fn grass_density_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[regia_core::schema::serde_field_names::<GrassDensityPaint>()])
}

/// `<map>.grassdensity.bin` is at its first version: no migration steps.
#[must_use]
pub fn grass_density_migration_sources() -> Vec<u32> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stroke(path: Vec<[f32; 2]>, target: f32) -> GrassDensityStroke {
        GrassDensityStroke {
            path,
            radius_m: 6.0,
            hardness: 0.5,
            strength: 1.0,
            target,
        }
    }

    #[test]
    fn a_stroke_is_deterministic_and_undo_redo_restore_bytes() {
        let mut a = GrassDensityPaint::new(64).expect("new");
        let mut b = a.clone();
        let s = stroke(vec![[-10.0, -4.0], [8.0, 3.0], [12.0, 12.0]], 0.0);
        let ua = a.stroke(&s, 64.0).expect("stroke");
        let ub = b.stroke(&s, 64.0).expect("stroke");
        assert_eq!(a, b, "same stroke, same bytes");
        assert_eq!(ua, ub);
        assert!(ua.changed > 0);
        let blank = GrassDensityPaint::new(64).expect("new");
        let painted = a.clone();
        assert!(ua.undo(&mut a));
        assert_eq!(a.texels(), blank.texels(), "undo restores the bytes");
        assert!(ua.redo(&mut a));
        assert_eq!(a.texels(), painted.texels(), "redo writes them again");
        assert_eq!(a.digest(), painted.digest());
        assert_ne!(a.digest(), blank.digest());
    }

    #[test]
    fn a_path_split_at_its_points_paints_the_same_image() {
        let mut whole = GrassDensityPaint::new(48).expect("new");
        let mut parts = whole.clone();
        let path = vec![[-12.0, 0.0], [0.0, 5.0], [10.0, -6.0]];
        whole.stroke(&stroke(path.clone(), 0.0), 48.0).expect("stroke");
        parts
            .stroke(&stroke(path[0..2].to_vec(), 0.0), 48.0)
            .expect("stroke");
        parts
            .stroke(&stroke(path[1..3].to_vec(), 0.0), 48.0)
            .expect("stroke");
        // Clearing to 0 at full strength: the overlap reaches 0 either way where the weight is 1;
        // where it is partial the second part starts from the first's value, so compare where
        // the whole path is at full weight.
        let full: Vec<usize> = (0..whole.texels().len())
            .filter(|&i| whole.texels()[i] == 0)
            .collect();
        assert!(!full.is_empty());
        for i in full {
            assert_eq!(parts.texels()[i], 0);
        }
    }

    #[test]
    fn restoring_brings_the_baked_density_back_and_outside_is_untouched() {
        let mut p = GrassDensityPaint::new(32).expect("new");
        p.stroke(&stroke(vec![[0.0, 0.0]], 0.0), 32.0).expect("clear");
        assert!(p.painted_texels() > 0);
        assert_eq!(p.sample(0.0, 0.0, 32.0), 0.0, "the centre is cleared");
        assert_eq!(p.sample(15.0, 15.0, 32.0), 1.0, "a far corner is untouched");
        let cleared = p.painted_texels();
        p.stroke(&stroke(vec![[0.0, 0.0]], 1.0), 32.0).expect("restore");
        assert_eq!(p.sample(0.0, 0.0, 32.0), 1.0, "a restore brings the centre back");
        assert!(p.painted_texels() < cleared, "and part of the falloff");
        let miss = p
            .stroke(&stroke(vec![[500.0, 500.0]], 0.0), 32.0)
            .expect("off the map");
        assert_eq!(miss.changed, 0);
        assert!(miss.before.is_empty());
    }

    #[test]
    fn bad_strokes_name_the_field() {
        let mut p = GrassDensityPaint::new(8).expect("new");
        let err = |s: GrassDensityStroke| p.clone().stroke(&s, 8.0).expect_err("bad").to_string();
        assert!(err(stroke(vec![], 0.0)).contains("path"));
        assert!(err(stroke(vec![[f32::NAN, 0.0]], 0.0)).contains("non-finite"));
        assert!(
            err(GrassDensityStroke {
                radius_m: 0.0,
                ..stroke(vec![[0.0, 0.0]], 0.0)
            })
            .contains("radius")
        );
        assert!(
            err(GrassDensityStroke {
                hardness: 2.0,
                ..stroke(vec![[0.0, 0.0]], 0.0)
            })
            .contains("hardness")
        );
        assert!(err(stroke(vec![[0.0, 0.0]], -1.0)).contains("target"));
        assert!(p.stroke(&stroke(vec![[0.0, 0.0]], 0.0), 0.0).is_err());
        assert_eq!(GrassDensityPaint::new(1), Err(GrassDensityError::BadSide(1)));
    }

    #[test]
    fn the_blob_round_trips_and_refuses_bad_files() {
        let mut p = GrassDensityPaint::new(16).expect("new");
        p.stroke(&stroke(vec![[1.0, 2.0]], 0.25), 16.0).expect("stroke");
        let bytes = p.to_bytes().expect("bytes");
        let back = GrassDensityPaint::from_bytes(&bytes).expect("reads");
        assert_eq!(back.texels(), p.texels());
        assert_eq!(back.to_bytes().expect("bytes"), bytes, "same image, same bytes");
        let mut newer = bytes.clone();
        newer[0] = 2;
        assert_eq!(
            GrassDensityPaint::from_bytes(&newer),
            Err(GrassDensityError::BadVersion { found: 2 })
        );
        assert!(GrassDensityPaint::from_bytes(b"").is_err());
        // A hostile side is refused before the texels are read.
        let hostile = postcard::to_allocvec(&(1u32, 1_000_000u32)).expect("bytes");
        assert_eq!(
            GrassDensityPaint::from_bytes(&hostile),
            Err(GrassDensityError::BadSide(1_000_000))
        );
        let mut short = bytes;
        short.truncate(short.len() - 3);
        assert!(GrassDensityPaint::from_bytes(&short).is_err());
    }

    #[test]
    fn edits_mark_the_image_unsaved_and_a_huge_stroke_is_refused() {
        let mut p = GrassDensityPaint::new(64).expect("new");
        assert!(!p.is_unsaved());
        let undo = p.stroke(&stroke(vec![[0.0, 0.0]], 0.0), 64.0).expect("stroke");
        assert!(p.is_unsaved());
        p.mark_saved();
        assert!(undo.undo(&mut p));
        assert!(p.is_unsaved(), "undo is an edit too");
        let loaded = GrassDensityPaint::from_bytes(&p.to_bytes().expect("bytes")).expect("load");
        assert!(!loaded.is_unsaved());
        assert_eq!(loaded, p, "equality ignores the edit counters");
        let mut big = GrassDensityPaint::new(4096).expect("new");
        let path: Vec<[f32; 2]> = (0..4096).map(|i| [i as f32 - 2048.0, 0.0]).collect();
        let err = big
            .stroke(
                &GrassDensityStroke {
                    path,
                    radius_m: 4096.0,
                    hardness: 0.5,
                    strength: 1.0,
                    target: 0.0,
                },
                4096.0,
            )
            .expect_err("too costly");
        assert!(err.to_string().contains("texel checks"), "{err}");
    }

    #[test]
    fn the_file_is_named_after_the_map() {
        assert_eq!(
            grass_density_path(Path::new("maps/switchbacks.level.ron")),
            Path::new("maps/switchbacks.grassdensity.bin")
        );
        assert_eq!(
            grass_density_path(Path::new("maps/arena")),
            Path::new("maps/arena.grassdensity.bin")
        );
    }

    #[test]
    fn multipliers_cover_every_class_and_multiply_into_the_base() {
        let set = GrassSet::summer();
        let mut p = GrassDensityPaint::new(4).expect("new");
        p.texels = vec![128; 16];
        let m = p.multipliers_for(&set, None);
        assert_eq!(m.by_class.len(), set.classes.len());
        let first = &set.classes[0].name;
        let mut base = GrassMultipliers::default();
        base.by_class.insert(
            first.clone(),
            MultiplierImage {
                side: 2,
                texels: vec![0, 255, 255, 255],
            },
        );
        base.by_class.insert(
            "other".into(),
            MultiplierImage {
                side: 1,
                texels: vec![7],
            },
        );
        let m = p.multipliers_for(&set, Some(&base));
        let img = &m.by_class[first];
        assert_eq!(img.texels[0], 0, "the base's zero quadrant stays zero");
        assert_eq!(img.texels[3], 128, "elsewhere the paint's 128");
        assert_eq!(
            m.by_class["other"].texels,
            vec![7],
            "classes the set lacks pass through"
        );
    }
}
