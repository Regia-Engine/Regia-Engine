// SPDX-License-Identifier: MPL-2.0
//! The grass mask: what the GPU placement reads per class, baked once on the CPU.
//!
//! One RGBA8 image per class over the whole map (row 0 at −Z, texel centres on a regular grid):
//!
//! - **R** density, `0..1` of the class's full density: the splat weights of the layers the
//!   class grows on (procedural slope/altitude bands, with painted weights layered on top,
//!   [`crate::TerrainLayers::blend_painted`]) × the slope and altitude bands × the density map
//!   × patchiness noise, zero inside holes and exclusion zones;
//! - **G** greenness: the fraction of blades that are green shoots;
//! - **B** blade height multiplier / 2;
//! - **A** a low-frequency tint variation (dry and damp patches).
//!
//! The bake is a pure function of its inputs: rows are computed independently (in parallel)
//! with a fixed operation order, so every machine bakes the same bytes.

use regia_app::prelude::*;

use super::{GrassClass, GrassError, GrassSet};
use crate::holes::HolesMask;
use crate::layer_paint::LayerWeightMap;
use crate::layers::{MAX_LAYERS, TerrainLayers};
use crate::sampler::TerrainMap;
use crate::scatter::placement::DensityMaps;

/// Largest mask side, texels.
pub const MAX_MASK_SIDE: u32 = 4096;

/// What the bake reads besides the set.
#[derive(Clone, Copy)]
pub struct GrassMaskInputs<'a> {
    /// The heightfield (slope and altitude).
    pub map: &'a TerrainMap,
    /// The terrain's layer stack (`None`: every class grows everywhere its bands allow).
    pub layers: Option<&'a TerrainLayers>,
    /// Painted splat weights over the layers (`None`: procedural weights only).
    pub painted: Option<&'a LayerWeightMap>,
    /// Density maps by the asset path the `.grass.ron` names.
    pub density_maps: Option<&'a DensityMaps>,
    /// Terrain holes (classes with `avoid_holes` grow nothing inside them).
    pub holes: Option<&'a HolesMask>,
    /// Mask texel edge, metres (the side is capped at [`MAX_MASK_SIDE`]).
    pub texel_m: f32,
    /// Per-class density multipliers (biomes, spec 65 § 8), multiplied into the density before
    /// quantization; `None` (the default): no multiplier.
    pub multipliers: Option<&'a GrassMultipliers>,
}

/// One class's density multiplier over the whole map: R8, `side × side` texels, row 0 at −Z (the
/// mask's layout). Any side works; the bake samples it at each mask texel centre (nearest).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MultiplierImage {
    /// Texels per side.
    pub side: u32,
    /// `side × side` bytes, `255` = ×1.
    pub texels: Vec<u8>,
}

impl MultiplierImage {
    /// The multiplier at world `(x, z)` on a map `world` metres wide (1 when the image is empty
    /// or malformed).
    #[must_use]
    pub fn sample(&self, x: f32, z: f32, world: f32) -> f32 {
        let side = self.side as usize;
        if side == 0 || self.texels.len() != side * side || world <= 0.0 {
            return 1.0;
        }
        let half = world * 0.5;
        let c = ((x + half) / world * side as f32).floor();
        let r = ((z + half) / world * side as f32).floor();
        let c = if c.is_nan() {
            0
        } else {
            (c.max(0.0) as usize).min(side - 1)
        };
        let r = if r.is_nan() {
            0
        } else {
            (r.max(0.0) as usize).min(side - 1)
        };
        f32::from(self.texels[r * side + c]) / 255.0
    }
}

/// Per-class grass density multipliers by class name (spec 65 § 8). Also a resource: the biome
/// generators of `regia_procgen` write it, and the grass setup re-bakes the classes it names.
#[derive(Resource, Clone, Debug, Default, PartialEq, Eq)]
pub struct GrassMultipliers {
    /// Multiplier image per class name.
    pub by_class: std::collections::BTreeMap<String, MultiplierImage>,
}

impl<'a> GrassMaskInputs<'a> {
    /// Inputs with only the heightfield and 1 m texels.
    #[must_use]
    pub fn new(map: &'a TerrainMap) -> Self {
        Self {
            map,
            layers: None,
            painted: None,
            density_maps: None,
            holes: None,
            texel_m: 1.0,
            multipliers: None,
        }
    }
}

/// The baked masks of a set, one image per class (see the module docs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrassMask {
    side: u32,
    /// `side × side × 4` bytes per class.
    images: Vec<Vec<u8>>,
    generation: u64,
}

fn smoothstep(lo: f32, hi: f32, v: f32) -> f32 {
    let t = ((v - lo) / (hi - lo)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// 1 inside `lo..=hi`, falling smoothly to 0 over `soft` outside it (an edge at 0 keeps flat
/// ground at full weight, unlike the layers' `band`).
fn soft_range(v: f32, lo: f32, hi: f32, soft: f32) -> f32 {
    smoothstep(lo - soft, lo, v) * (1.0 - smoothstep(hi, hi + soft, v))
}

fn unorm8(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
}

struct ClassBake<'a> {
    class: &'a GrassClass,
    seed: u32,
    density_map: Option<&'a crate::DensityMap>,
    multiplier: Option<&'a MultiplierImage>,
}

impl GrassMask {
    /// Bakes the masks of every class of `set`.
    ///
    /// # Errors
    ///
    /// [`GrassError`] when the set is invalid or names a density map `inputs` lacks.
    pub fn build(set: &GrassSet, inputs: &GrassMaskInputs<'_>) -> Result<Self, GrassError> {
        let _span = tracing::info_span!("grass_mask_build", classes = set.classes.len()).entered();
        set.validate(inputs.layers.map(TerrainLayers::count))?;
        let world = inputs.map.world_width();
        let texel = if inputs.texel_m.is_finite() && inputs.texel_m > 0.0 {
            inputs.texel_m
        } else {
            1.0
        };
        let side = ((world / texel).ceil() as u32).clamp(2, MAX_MASK_SIDE);
        let mut bakes = Vec::with_capacity(set.classes.len());
        for class in &set.classes {
            let density_map =
                match class.density_map.as_deref() {
                    None => None,
                    Some(path) => Some(inputs.density_maps.and_then(|maps| maps.get(path)).ok_or_else(
                        || GrassError::Asset {
                            path: path.to_owned(),
                            reason: "not supplied to the mask bake".into(),
                        },
                    )?),
                };
            bakes.push(ClassBake {
                class,
                seed: class.stream_seed(set.seed),
                density_map,
                multiplier: inputs.multipliers.and_then(|m| m.by_class.get(&class.name)),
            });
        }
        let row_bytes = side as usize * 4;
        let mut images: Vec<Vec<u8>> = vec![vec![0u8; row_bytes * side as usize]; bakes.len()];
        let threads = std::thread::available_parallelism()
            .map(std::num::NonZeroUsize::get)
            .unwrap_or(1)
            .clamp(1, 16);
        let rows_per = (side as usize).div_ceil(threads).max(1);
        for (bake, image) in bakes.iter().zip(images.iter_mut()) {
            std::thread::scope(|scope| {
                for (chunk_index, chunk) in image.chunks_mut(rows_per * row_bytes).enumerate() {
                    let first_row = chunk_index * rows_per;
                    scope.spawn(move || {
                        for (r, row) in chunk.chunks_mut(row_bytes).enumerate() {
                            bake_row(bake, inputs, side, (first_row + r) as u32, row);
                        }
                    });
                }
            });
        }
        Ok(Self {
            side,
            images,
            generation: 1,
        })
    }

    /// Texels per side.
    #[must_use]
    pub fn side(&self) -> u32 {
        self.side
    }

    /// Classes baked.
    #[must_use]
    pub fn classes(&self) -> usize {
        self.images.len()
    }

    /// One class's RGBA8 image, row-major, row 0 at −Z.
    #[must_use]
    pub fn image(&self, class: usize) -> Option<&[u8]> {
        self.images.get(class).map(Vec::as_slice)
    }

    /// Bumped by every edit ([`Self::clear_disc`], [`Self::scale_density`]); the GPU layer
    /// re-uploads when it changes.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Continues the generation count of `previous`, so a re-baked mask that replaces it is seen
    /// as changed (a fresh bake starts at 1).
    pub(crate) fn follow_generation(&mut self, previous: &Self) {
        self.generation = previous.generation.max(self.generation) + 1;
    }

    /// The sum of class `class`'s density channel (R) over every texel: the GPU keeps a clump in
    /// proportion to it, so it is the CPU-side measure of how much of a class draws (0 for an
    /// unknown class).
    #[must_use]
    pub fn density_total(&self, class: usize) -> u64 {
        self.images
            .get(class)
            .map_or(0, |image| image.iter().step_by(4).map(|&r| u64::from(r)).sum())
    }

    /// The mask texel holding world `(x, z)` on a map `world` metres wide.
    fn texel(&self, x: f32, z: f32, world: f32) -> (i64, i64) {
        let half = world * 0.5;
        let u = (x + half) / world * self.side as f32;
        let v = (z + half) / world * self.side as f32;
        (u.floor() as i64, v.floor() as i64)
    }

    /// Mean density (R, `0..1`) of class `class` over the texels whose centres lie within
    /// `radius` of `(x, z)` (tests, the agent report).
    #[must_use]
    pub fn mean_density(&self, class: usize, x: f32, z: f32, radius: f32, world: f32) -> f32 {
        let Some(image) = self.image(class) else {
            return 0.0;
        };
        let (mut sum, mut count) = (0.0f64, 0u32);
        self.for_disc(x, z, radius, world, |i| {
            sum += f64::from(image[i]) / 255.0;
            count += 1;
        });
        if count == 0 {
            0.0
        } else {
            (sum / f64::from(count)) as f32
        }
    }

    fn for_disc(&self, x: f32, z: f32, radius: f32, world: f32, mut f: impl FnMut(usize)) {
        let texel = world / self.side as f32;
        let (c0, r0) = self.texel(x - radius, z - radius, world);
        let (c1, r1) = self.texel(x + radius, z + radius, world);
        let half = world * 0.5;
        for r in r0.max(0)..=r1.min(i64::from(self.side) - 1) {
            for c in c0.max(0)..=c1.min(i64::from(self.side) - 1) {
                let cx = (c as f32 + 0.5) * texel - half;
                let cz = (r as f32 + 0.5) * texel - half;
                if (cx - x) * (cx - x) + (cz - z) * (cz - z) <= radius * radius {
                    f((r as usize * self.side as usize + c as usize) * 4);
                }
            }
        }
    }

    /// Scales the density of every class within `radius` of `(x, z)` by `factor` (`0` clears:
    /// a crater, a burnt patch, a building pad). Returns the texels touched.
    pub fn scale_density(&mut self, x: f32, z: f32, radius: f32, factor: f32, world: f32) -> u32 {
        let factor = if factor.is_finite() {
            factor.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let mut touched = Vec::new();
        self.for_disc(x, z, radius, world, |i| touched.push(i));
        for image in &mut self.images {
            for &i in &touched {
                image[i] = unorm8(f32::from(image[i]) / 255.0 * factor);
            }
        }
        if !touched.is_empty() {
            self.generation += 1;
        }
        touched.len() as u32
    }

    /// Clears every class within `radius` of `(x, z)` ([`Self::scale_density`] with 0).
    pub fn clear_disc(&mut self, x: f32, z: f32, radius: f32, world: f32) -> u32 {
        self.scale_density(x, z, radius, 0.0, world)
    }
}

fn bake_row(bake: &ClassBake<'_>, inputs: &GrassMaskInputs<'_>, side: u32, row: u32, out: &mut [u8]) {
    let class = bake.class;
    let map = inputs.map;
    let world = map.world_width();
    let half = world * 0.5;
    let texel = world / side as f32;
    let z = (row as f32 + 0.5) * texel - half;
    // The mask is in the map's own frame; the map reads world XZ (its grid origin, ADR-8179).
    let [ox, oz] = map.grid_origin();
    let noise_seed = bake.seed;
    let range = {
        let (lo, hi) = map.min_max_height();
        [lo, hi]
    };
    for col in 0..side {
        let x = (col as f32 + 0.5) * texel - half;
        let n = map.normal_at(x + ox, z + oz);
        let slope = n[1].clamp(-1.0, 1.0).acos();
        let altitude = map.sample_bilinear(x + ox, z + oz);
        // Layer rules.
        let (mut density, mut green, mut height) = (1.0f32, 0.0f32, 1.0f32);
        if !class.layers.is_empty() {
            let weights = match inputs.layers {
                Some(layers) => {
                    let painted = inputs.painted.map(|p| {
                        // `saturating_sub` (not `max(2) - 1`): a 1-texel-wide map has factor 0
                        // and paints its texel 0 at every world point; with factor 1 the +X/+Z
                        // half rounds past the map and reads as unpainted.
                        let pc = ((x + half) / world * p.width().saturating_sub(1) as f32).round();
                        let pr = ((z + half) / world * p.height().saturating_sub(1) as f32).round();
                        p.sample(pc.max(0.0) as u32, pr.max(0.0) as u32)
                    });
                    match painted {
                        Some(p) => layers.blend_painted_in(slope, altitude, Some(range), &p),
                        None => layers.weights_in(slope, altitude, Some(range)),
                    }
                }
                None => {
                    let mut w = vec![0.0; MAX_LAYERS];
                    w[0] = 1.0;
                    w
                }
            };
            density = 0.0;
            let mut listed = 0.0f32;
            let mut height_sum = 0.0f32;
            for rule in &class.layers {
                let w = weights.get(usize::from(rule.layer)).copied().unwrap_or(0.0);
                density += w * rule.density;
                green += w * rule.green;
                height_sum += w * rule.height;
                listed += w;
            }
            height = if listed > 1e-4 { height_sum / listed } else { 1.0 };
        }
        density *= soft_range(slope, class.slope_min, class.slope_max, 0.08);
        density *= soft_range(altitude, class.height_min, class.height_max, 2.0);
        if let Some(dmap) = bake.density_map {
            density *= dmap.sample(x, z, world);
        }
        if class.noise_amount > 0.0 {
            let s = 1.0 / class.noise_scale;
            let v = crate::noise::fbm2(x * s, z * s, noise_seed);
            density *= 1.0 - class.noise_amount * (1.0 - smoothstep(0.25, 0.7, v));
        }
        if class.avoid_holes && inputs.holes.is_some_and(|h| h.contains(x, z)) {
            density = 0.0;
        }
        if class.exclusions.iter().any(|e| e.contains(x, z)) {
            density = 0.0;
        }
        if let Some(m) = bake.multiplier {
            density *= m.sample(x, z, world);
        }
        let tint = crate::noise::fbm2(x / 23.0, z / 23.0, noise_seed ^ 0x7f4a_7c15);
        let i = col as usize * 4;
        out[i] = unorm8(density);
        out[i + 1] = unorm8(class.green_fraction + green);
        out[i + 2] = unorm8(height * 0.5);
        out[i + 3] = unorm8(tint);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GrassLayerRule, LayerDesc, ScatterExclusion};

    fn slope_map() -> TerrainMap {
        // Flat on −X, a 63° ramp on +X.
        let n = 65u32;
        let world = 128.0;
        let mut heights = Vec::new();
        for _row in 0..n {
            for col in 0..n {
                let x = col as f32 / (n - 1) as f32 * world - world * 0.5;
                heights.push(if x > 0.0 { 2.0 * x } else { 0.0 });
            }
        }
        TerrainMap::from_heights(n, n, heights, world, 1.0, 0.0).expect("map")
    }

    fn class() -> GrassClass {
        GrassClass {
            noise_amount: 0.0,
            ..GrassClass::winter_dead()
        }
    }

    #[test]
    fn slope_band_clears_steep_ground_and_the_bake_is_deterministic() {
        let map = slope_map();
        let set = GrassSet {
            classes: vec![class()],
            ..GrassSet::default()
        };
        let inputs = GrassMaskInputs {
            texel_m: 2.0,
            ..GrassMaskInputs::new(&map)
        };
        let a = GrassMask::build(&set, &inputs).expect("mask");
        let b = GrassMask::build(&set, &inputs).expect("mask");
        assert_eq!(a, b);
        assert_eq!(a.side(), 64);
        let flat = a.mean_density(0, -40.0, 0.0, 10.0, 128.0);
        let steep = a.mean_density(0, 40.0, 0.0, 10.0, 128.0);
        assert!(flat > 0.95, "flat {flat}");
        assert!(steep < 0.05, "steep {steep}");
    }

    #[test]
    fn layer_rules_drive_density_and_greenness() {
        let map = slope_map();
        // Layer 0 on flat ground, layer 1 on slopes over 0.5 rad.
        let layers = TerrainLayers {
            layers: vec![
                LayerDesc {
                    name: "meadow".into(),
                    slope_min: 0.0,
                    slope_max: 0.4,
                    blend_sharpness: 0.05,
                    ..LayerDesc::ground()
                },
                LayerDesc {
                    name: "bank".into(),
                    slope_min: 0.5,
                    slope_max: 1.6,
                    blend_sharpness: 0.05,
                    ..LayerDesc::ground()
                },
            ],
            ..TerrainLayers::default()
        };
        let mut c = class();
        c.slope_max = 1.6;
        c.green_fraction = 0.0;
        c.layers = vec![
            GrassLayerRule {
                layer: 0,
                density: 0.0,
                green: 0.0,
                height: 1.0,
            },
            GrassLayerRule {
                layer: 1,
                density: 1.0,
                green: 0.8,
                height: 1.5,
            },
        ];
        let set = GrassSet {
            classes: vec![c],
            ..GrassSet::default()
        };
        let inputs = GrassMaskInputs {
            layers: Some(&layers),
            texel_m: 2.0,
            ..GrassMaskInputs::new(&map)
        };
        let mask = GrassMask::build(&set, &inputs).expect("mask");
        assert!(mask.mean_density(0, -40.0, 0.0, 8.0, 128.0) < 0.05);
        assert!(mask.mean_density(0, 40.0, 0.0, 8.0, 128.0) > 0.6);
        let image = mask.image(0).expect("image");
        // A texel on the ramp: green ≈ 0.8, height ≈ 1.5 / 2.
        let (c, r) = (52usize, 32usize);
        let i = (r * 64 + c) * 4;
        assert!(
            (i32::from(image[i + 1]) - 204).abs() <= 8,
            "green {}",
            image[i + 1]
        );
        assert!(
            (i32::from(image[i + 2]) - 191).abs() <= 8,
            "height {}",
            image[i + 2]
        );
    }

    #[test]
    fn exclusions_and_edits_clear_grass() {
        let map = slope_map();
        let mut c = class();
        c.exclusions = vec![ScatterExclusion {
            points: vec![[-40.0, -40.0], [-40.0, 40.0]],
            radius: 4.0,
        }];
        let set = GrassSet {
            classes: vec![c],
            ..GrassSet::default()
        };
        let mut mask = GrassMask::build(
            &set,
            &GrassMaskInputs {
                texel_m: 1.0,
                ..GrassMaskInputs::new(&map)
            },
        )
        .expect("mask");
        assert!(mask.mean_density(0, -40.0, 0.0, 2.0, 128.0) < 0.01);
        assert!(mask.mean_density(0, -20.0, 0.0, 2.0, 128.0) > 0.9);
        let generation = mask.generation();
        let touched = mask.clear_disc(-20.0, 0.0, 3.0, 128.0);
        assert!(touched > 20);
        assert!(mask.generation() > generation);
        assert!(mask.mean_density(0, -20.0, 0.0, 2.0, 128.0) < 0.01);
    }

    #[test]
    fn a_one_by_one_paint_map_paints_the_whole_map() {
        // A 1×1 LayerWeightMap is valid (level loads always see the heightfield's ≥ 2 size,
        // but the direct GrassMaskInputs.painted path accepts it). Its single texel must
        // apply at every world point; sampling with `width - 1` as the column factor rounds
        // the +X half to column 1, past the map, and silently fell back to procedural there.
        // Flat map: the slope band must not clear either half, so the only signal left is
        // the paint sampling (slope_map's +X ramp would zero the east disc regardless).
        let map = TerrainMap::from_heights(65, 65, vec![0.0f32; 65 * 65], 128.0, 1.0, 0.0).expect("map");
        // Procedural weights give layer 0 everything on flat ground; the paint says layer 1.
        // The class grows only on layer 1, so any fallback to procedural kills the grass.
        let layers = TerrainLayers {
            layers: vec![
                LayerDesc {
                    name: "meadow".into(),
                    ..LayerDesc::ground()
                },
                LayerDesc {
                    name: "painted".into(),
                    // Unreachable procedurally on flat ground: only the paint can grow it.
                    slope_min: 0.5,
                    ..LayerDesc::ground()
                },
            ],
            ..TerrainLayers::default()
        };
        let mut c = class();
        c.layers = vec![
            GrassLayerRule {
                layer: 0,
                density: 0.0,
                green: 0.0,
                height: 1.0,
            },
            GrassLayerRule {
                layer: 1,
                density: 1.0,
                green: 0.0,
                height: 1.0,
            },
        ];
        let set = GrassSet {
            classes: vec![c],
            ..GrassSet::default()
        };
        let mut painted = LayerWeightMap::new(1, 1, 2).expect("paint map");
        painted.set(1, 0, 0, 255);
        let inputs = GrassMaskInputs {
            layers: Some(&layers),
            painted: Some(&painted),
            texel_m: 2.0,
            ..GrassMaskInputs::new(&map)
        };
        let mask = GrassMask::build(&set, &inputs).expect("mask");
        let west = mask.mean_density(0, -40.0, 0.0, 8.0, 128.0);
        let east = mask.mean_density(0, 40.0, 0.0, 8.0, 128.0);
        assert!(
            west > 0.9,
            "−X texel centre: the painted weight must cover it, got {west}"
        );
        assert!(
            (east - west).abs() < 0.05 && east > 0.9,
            "the 1×1 paint map must paint the +X half like the −X half: west {west}, east {east}"
        );
    }

    #[test]
    fn a_missing_density_map_is_an_error() {
        let map = slope_map();
        let mut c = class();
        c.density_map = Some("maps/grass.png".into());
        let set = GrassSet {
            classes: vec![c],
            ..GrassSet::default()
        };
        let err = GrassMask::build(&set, &GrassMaskInputs::new(&map)).expect_err("missing map");
        assert!(err.to_string().contains("maps/grass.png"), "{err}");
    }

    #[test]
    fn a_multiplier_of_one_keeps_the_bake_byte_identical_and_scales_where_lower() {
        let map = slope_map();
        let set = GrassSet {
            classes: vec![class()],
            ..GrassSet::winter()
        };
        let name = set.classes[0].name.clone();
        let plain = GrassMask::build(
            &set,
            &GrassMaskInputs {
                texel_m: 2.0,
                ..GrassMaskInputs::new(&map)
            },
        )
        .expect("mask");
        // ×1 on the left half (rows split by column), ×0 on the right half.
        let side = 16u32;
        let texels = (0..side * side)
            .map(|i| if i % side < side / 2 { 255 } else { 0 })
            .collect();
        let mut mult = GrassMultipliers::default();
        mult.by_class.insert(name, MultiplierImage { side, texels });
        let scaled = GrassMask::build(
            &set,
            &GrassMaskInputs {
                texel_m: 2.0,
                multipliers: Some(&mult),
                ..GrassMaskInputs::new(&map)
            },
        )
        .expect("mask");
        let (a, b) = (plain.image(0).expect("image"), scaled.image(0).expect("image"));
        let row = plain.side() as usize * 4;
        for (ra, rb) in a.chunks(row).zip(b.chunks(row)) {
            let half = row / 2;
            assert_eq!(&ra[..half], &rb[..half], "left half byte-identical");
            assert!(rb[half..].chunks(4).all(|t| t[0] == 0), "right half cleared");
        }
    }
}
