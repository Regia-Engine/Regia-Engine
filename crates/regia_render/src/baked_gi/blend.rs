// SPDX-License-Identifier: MPL-2.0
//! Several baked GI volumes at once (ADR-1097, `book/agents/baked-gi-seams.md`): adjacent
//! volumes, a streamed level's tile beside the map's own, a small set-piece bake inside a large
//! one. Mirrored by `shaders/baked_gi_set.wgsl` (`static_gi_set`); keep the two in step.
//!
//! Each volume is a [`BakedGiLayer`]: the volume, a priority, a blend distance and a weight (the
//! streaming fade). At a point every layer's own lookup ([`super::sample_static_gi`]: its light
//! normalized to full strength, and its coverage) is weighted by
//!
//! `w = weight · coverage · edge`
//!
//! where `edge` is 1 everywhere for a blend distance of 0, and otherwise ramps linearly from 0
//! at the volume's footprint edge to 1 at `blend_distance` inside it, the X, Y and Z ramps
//! multiplied ([`BakedGiFootprint::edge`]). Y is a closed face of a brick box. A terrain layer
//! is open in Y (its own coverage is the column, full below the probes and fading above the
//! top), so its Y factor is 1 and a horizontal seam is unchanged. Layers of equal priority
//! blend by their weights, normalized; the tier's coverage is `min(Σw, 1)`:
//!
//! - Two tiles cut one column apart (no blend distance) sum to full weight across the seam
//!   cell, each tile's coverage fading over its own fringe, so the seam reads as one volume.
//! - Tiles that overlap by `o` with `blend_distance = o` sum to exactly 1 over the overlap (a
//!   partition of unity, in X, Z, in Y for two brick boxes stacked on a horizontal face, and
//!   at corners where four meet), so the light moves linearly from one to the other. A blend
//!   distance above the overlap dips the coverage there.
//!
//! Priorities composite in order, highest first, each taking what the higher ones left:
//! `take = (1 − acc) · min(Σw, 1)`. Every factor is continuous in the point, so the light is
//! continuous across every seam.
//!
//! The blend within a priority is a partition of unity: compactly supported weights, normalized
//! by their sum (Babuška & Melenk, "The Partition of Unity Method", IJNME 1997; the normalized
//! weights of Shepard, "A two-dimensional interpolation function for irregularly-spaced data",
//! ACM 1968). Between priorities it is Porter & Duff's "over" ("Compositing Digital Images",
//! SIGGRAPH 1984) on coverage. Original code.

use std::sync::Arc;

use regia_asset::baked_gi::{BakedGiVolume, GI_COMPONENTS, ShRgb};

use super::{
    BakedGiFrameLights, BakedGiSettings, GiEval, LayerSplit, StaticGiSample, VolumeSh, component_scales,
    sun_relight, volume_sh,
};

/// Most volumes one frame blends (the screen pass's uniform array). Layers past it, in priority
/// order, are dropped and counted ([`super::BakedGiStatus::volumes_dropped`]).
pub const MAX_BAKED_GI_VOLUMES: usize = 8;

/// One volume of a blended set ([`crate::FrameRenderer::set_baked_gi_layers`],
/// [`sample_static_gi_blend`]).
#[derive(Clone, Debug)]
pub struct BakedGiLayer {
    /// The volume.
    pub volume: Arc<BakedGiVolume>,
    /// Where layers overlap the higher priority lights first and the lower ones fill what it
    /// leaves; equal priorities blend by weight. The level's own volume
    /// ([`crate::FrameRenderer::set_baked_gi`]) has priority 0.
    pub priority: i32,
    /// Meters inside the volume's footprint over which it fades in from its edge (0: no fade,
    /// the volume's own one-cell fringe outside its outer probes only). Set it to the overlap
    /// of two tiles to spread the hand-over across it, or on a nested volume to the width of
    /// its hand-over to the one around it. It fades every closed edge, so a tile at the world's
    /// edge fades there too. X and Z are always closed. Y is closed on a brick box and open on
    /// a terrain layer, so a blend distance chosen for a horizontal overlap does not fade the
    /// ground or the sky.
    pub blend_distance: f32,
    /// Multiplier on the layer's weight, 0–1: a streamed tile fades in from 0 and out to 0.
    pub weight: f32,
}

impl BakedGiLayer {
    /// A layer of priority 0, no blend distance, full weight.
    #[must_use]
    pub fn new(volume: Arc<BakedGiVolume>) -> Self {
        Self {
            volume,
            priority: 0,
            blend_distance: 0.0,
            weight: 1.0,
        }
    }

    /// The layer can light a point: a finite weight above 0 and a finite blend distance ≥ 0.
    #[must_use]
    pub fn usable(&self) -> bool {
        self.weight.is_finite()
            && self.weight > 0.0
            && self.blend_distance.is_finite()
            && self.blend_distance >= 0.0
    }

    /// The weight clamped to 0–1.
    #[must_use]
    pub(crate) fn clamped_weight(&self) -> f32 {
        self.weight.clamp(0.0, 1.0)
    }
}

/// A volume's footprint (the [`BakedGiLayer`] edge weight): the box of its probes' nominal
/// positions in X and Z, the vertical span of a closed brick box, and how far past the box
/// the volume's lookup can still cover.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BakedGiFootprint {
    /// Smallest world X, Z of a probe.
    pub min: [f32; 2],
    /// Largest world X, Z of a probe.
    pub max: [f32; 2],
    /// Along X and Z, the most a point outside the box can be from it and still have coverage:
    /// the terrain layer's column spacing (its fringe), or half the largest brick spacing (an
    /// interior brick tests the point pushed half a cell along its normal), whichever is more.
    pub reach: [f32; 2],
    /// Smallest nominal probe Y of a closed brick box. Unused when [`Self::y_closed`] is false.
    pub min_y: f32,
    /// Largest nominal probe Y of a closed brick box. Unused when [`Self::y_closed`] is false.
    pub max_y: f32,
    /// Along Y, half the largest brick spacing: how far past the box an interior brick can still
    /// cover when the point is pushed half a cell along its normal. The blend's reach skip stays
    /// on X and Z; a blend distance of 0 must keep that fringe, and a positive one cuts Y in
    /// [`Self::edge`].
    pub reach_y: f32,
    /// The volume is a brick box, closed on Y, so [`Self::edge`] multiplies a Y ramp. A terrain
    /// layer is open in Y (the factor is 1): its coverage holds below the probes and fades above
    /// the top by its own rule, and a horizontal seam must not dip.
    pub y_closed: bool,
}

impl BakedGiFootprint {
    /// The footprint of `volume`: its terrain layer's columns and its bricks' boxes. `None` for
    /// a volume with neither.
    #[must_use]
    pub fn of(volume: &BakedGiVolume) -> Option<Self> {
        let mut min = [f32::INFINITY; 2];
        let mut max = [f32::NEG_INFINITY; 2];
        let mut reach = [0.0f32; 2];
        let mut min_y = f32::INFINITY;
        let mut max_y = f32::NEG_INFINITY;
        let mut reach_y = 0.0f32;
        // A terrain layer has no floor and no ceiling in the blend: coverage holds down the
        // column and fades above the top probes on its own. Closing Y here would dip every
        // horizontal seam whose blend distance reaches the sample.
        let terrain = volume
            .terrain
            .as_ref()
            .filter(|t| t.counts[0] > 0 && t.counts[1] > 0);
        let has_terrain = terrain.is_some();
        if let Some(t) = terrain {
            for a in 0..2 {
                let far = t.origin[a] + (t.counts[a] - 1) as f32 * t.spacing[a];
                min[a] = min[a].min(t.origin[a]);
                max[a] = max[a].max(far);
                reach[a] = reach[a].max(t.spacing[a]);
            }
        }
        for b in &volume.bricks {
            let half = 0.5 * b.spacing.iter().fold(0.0f32, |m, s| m.max(*s));
            reach = reach.map(|r| r.max(half));
            reach_y = reach_y.max(half);
            let far_y = b.origin[1] + b.counts[1].saturating_sub(1) as f32 * b.spacing[1];
            min_y = min_y.min(b.origin[1]);
            max_y = max_y.max(far_y);
            // World X and Z are the brick's axes 0 and 2.
            for (a, axis) in [(0usize, 0usize), (1, 2)] {
                let far = b.origin[axis] + b.counts[axis].saturating_sub(1) as f32 * b.spacing[axis];
                min[a] = min[a].min(b.origin[axis]);
                max[a] = max[a].max(far);
            }
        }
        // A single probe layer (`min_y == max_y`) has no inside along Y: a closed ramp would
        // weigh it 0 everywhere, so it stays open in Y like a terrain layer.
        let y_closed = !has_terrain && min_y < max_y;
        let (min_y, max_y, reach_y) = if y_closed {
            (min_y, max_y, reach_y)
        } else {
            (0.0, 0.0, 0.0)
        };
        (min[0] <= max[0] && min[1] <= max[1]).then_some(Self {
            min,
            max,
            reach,
            min_y,
            max_y,
            reach_y,
            y_closed,
        })
    }

    /// The volume's lookup can cover world `(x, z)`: it is inside the box grown by
    /// [`Self::reach`]. Outside, the coverage is 0, so the blend skips the volume there (the same
    /// light, without the lookup's cost).
    #[must_use]
    pub fn reaches(&self, x: f32, z: f32) -> bool {
        x >= self.min[0] - self.reach[0]
            && x <= self.max[0] + self.reach[0]
            && z >= self.min[1] - self.reach[1]
            && z <= self.max[1] + self.reach[1]
    }

    /// The edge weight at world `(x, y, z)` for `blend_distance`: per closed axis
    /// `clamp(d / blend, 0, 1)` with `d` the distance inside the box (negative outside), the
    /// axes multiplied. X and Z are always closed. Y is closed when [`Self::y_closed`] (a brick
    /// box) and open for a terrain layer, whose factor is 1. The weight is 1 everywhere when
    /// `blend_distance` is 0, including outside the box and on Y: the volume's own coverage is
    /// the cut.
    #[must_use]
    pub fn edge(&self, x: f32, y: f32, z: f32, blend_distance: f32) -> f32 {
        if blend_distance <= 0.0 {
            return 1.0;
        }
        let mut e = 1.0f32;
        for (a, q) in [x, z].into_iter().enumerate() {
            let d = (q - self.min[a]).min(self.max[a] - q);
            e *= (d / blend_distance).clamp(0.0, 1.0);
        }
        e * self.vertical_ramp(y, blend_distance)
    }

    /// The Y factor of [`Self::edge`]. 1 when the blend distance is 0 or the volume is open in Y.
    fn vertical_ramp(&self, y: f32, blend_distance: f32) -> f32 {
        if !self.y_closed || blend_distance <= 0.0 {
            return 1.0;
        }
        let d = (y - self.min_y).min(self.max_y - y);
        (d / blend_distance).clamp(0.0, 1.0)
    }
}

/// The per-edge fade of a split part (ADR-2618, `regia_asset::baked_gi_split`) beside its
/// installed neighbors: across each edge with a neighbor of the same set in the same priority
/// tier, the weight ramps from 1 to 0 over the band at the part's split rectangle, so two parts
/// sum to exactly 1 over their band (and four to 1 where four meet); every other edge keeps the
/// layer's own ramp ([`BakedGiFootprint::edge`]). Mirrored by `baked_gi_set.wgsl`
/// `bgi_layer_edge` (`tier.z` = 1).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SplitFade {
    /// Edge positions: min x, min z, max x, max z (meters).
    pub edges: [f32; 4],
    /// Ramp width at each edge (meters; 0: that edge does not fade).
    pub bands: [f32; 4],
    /// Edges whose neighbor is installed (`-x, -z, +x, +z` order).
    pub joined: [bool; 4],
}

impl SplitFade {
    /// The edge weight at world `(x, z)`: per axis the smaller of the two edge ramps
    /// (`clamp(d / band, 0, 1)`, 1 at an edge of band 0), the two axes multiplied.
    #[must_use]
    pub fn edge(&self, x: f32, z: f32) -> f32 {
        let ramp = |d: f32, band: f32| {
            if band <= 0.0 {
                1.0
            } else {
                (d / band).clamp(0.0, 1.0)
            }
        };
        let ex = ramp(x - self.edges[0], self.bands[0]).min(ramp(self.edges[2] - x, self.bands[2]));
        let ez = ramp(z - self.edges[1], self.bands[1]).min(ramp(self.edges[3] - z, self.bands[3]));
        ex * ez
    }
}

/// For each of `ordered` (blend order), its [`SplitFade`]: `Some` for a split part with at least
/// one neighbor of its own set installed in the same priority tier, `None` otherwise (the layer's
/// usual edge weight). A part whose neighbor is missing (not installed, or dropped) lights out to
/// its own extent on that side.
#[must_use]
pub fn split_fades(ordered: &[&BakedGiLayer]) -> Vec<Option<SplitFade>> {
    ordered
        .iter()
        .map(|layer| {
            let split = layer.volume.split.as_ref()?;
            let joined: [bool; 4] = std::array::from_fn(|e| {
                split.neighbor(e).is_some_and(|n| {
                    ordered.iter().any(|o| {
                        o.priority == layer.priority
                            && o.volume
                                .split
                                .as_ref()
                                .is_some_and(|s| s.group == split.group && s.index == n)
                    })
                })
            });
            if !joined.contains(&true) {
                return None;
            }
            let fp = BakedGiFootprint::of(&layer.volume).unwrap_or_default();
            let outer = [fp.min[0], fp.min[1], fp.max[0], fp.max[1]];
            Some(SplitFade {
                edges: std::array::from_fn(|e| if joined[e] { split.rect[e] } else { outer[e] }),
                bands: std::array::from_fn(|e| {
                    if joined[e] {
                        split.band[e]
                    } else {
                        layer.blend_distance
                    }
                }),
                joined,
            })
        })
        .collect()
}

/// One volume's lookup at a point: its per-component SH scaled to full strength (× intensity /
/// the visibility-weighted share, as `static_gi` returns it), its coverage, its visibility and
/// its layer split.
/// `None` where it covers nothing. The single-volume lookup itself (`super::volume_sh`), so a
/// layer blends exactly what [`super::sample_static_gi_relit`] would light alone.
fn layer_lookup(
    volume: &BakedGiVolume,
    settings: &BakedGiSettings,
    scales: &[[f32; 3]; GI_COMPONENTS],
    ground: f32,
    p: [f32; 3],
    n: [f32; 3],
    v: [f32; 3],
) -> Option<VolumeSh> {
    volume_sh(volume, settings, scales, ground, p, n, v)
}

/// The layers a frame blends, in the order the GPU sees them: usable, sorted by priority
/// (highest first; equal priorities keep their order), at most [`MAX_BAKED_GI_VOLUMES`].
/// Returns them and how many usable layers were dropped past the cap.
#[must_use]
pub fn order_layers(layers: &[BakedGiLayer]) -> (Vec<&BakedGiLayer>, usize) {
    let mut ordered: Vec<&BakedGiLayer> = layers.iter().filter(|l| l.usable()).collect();
    // A stable sort: equal priorities keep the caller's order (the level's own volume first).
    ordered.sort_by_key(|l| std::cmp::Reverse(l.priority));
    let dropped = ordered.len().saturating_sub(MAX_BAKED_GI_VOLUMES);
    ordered.truncate(MAX_BAKED_GI_VOLUMES);
    (ordered, dropped)
}

/// `true` when the blend of `ordered` is exactly the first layer's own lookup (one layer, full
/// weight, no blend distance): the CPU and the shader then take the single-volume path
/// unchanged, bit for bit.
#[must_use]
pub(crate) fn single_volume(ordered: &[&BakedGiLayer]) -> bool {
    matches!(ordered, [only] if only.clamped_weight() >= 1.0 && only.blend_distance <= 0.0)
}

/// The blended baked irradiance at `p` (unit normal `n`, unit `v` toward the viewer) of
/// several volumes, each with its own component factors for the frame's `lights`
/// ([`component_scales`]): the CPU mirror of the screen pass's `static_gi_set`, for gameplay
/// queries and tests. One layer at full weight and no blend distance is exactly
/// [`super::sample_static_gi`]. Unusable layers and layers past [`MAX_BAKED_GI_VOLUMES`] (in
/// priority order) are ignored, as on the GPU.
#[must_use]
pub fn sample_static_gi_blend(
    layers: &[BakedGiLayer],
    settings: &BakedGiSettings,
    lights: &BakedGiFrameLights,
    p: [f32; 3],
    n: [f32; 3],
    v: [f32; 3],
) -> StaticGiSample {
    let (ordered, _) = order_layers(layers);
    if single_volume(&ordered) {
        let volume = &ordered[0].volume;
        let scales = component_scales(volume, settings, lights);
        let relight = sun_relight(volume, settings, lights);
        return super::sample_static_gi_relit(volume, settings, &scales, &relight, p, n, v);
    }
    let mut out = [[[0.0f32; 4]; 3]; GI_COMPONENTS];
    let mut acc = 0.0f32;
    let mut vis = 0.0f32;
    let mut split = LayerSplit::default();
    let fades = split_fades(&ordered);
    let mut k = 0;
    while k < ordered.len() && acc < 1.0 {
        let tier = ordered[k].priority;
        let mut tier_sh = [[[0.0f32; 4]; 3]; GI_COMPONENTS];
        let mut tier_w = 0.0f32;
        let mut tier_vis = 0.0f32;
        let mut tier_split = LayerSplit::default();
        while k < ordered.len() && ordered[k].priority == tier {
            let layer = ordered[k];
            let fade = fades[k];
            k += 1;
            // A volume without probes has an empty footprint (and covers nothing), as on the GPU.
            let footprint = BakedGiFootprint::of(&layer.volume).unwrap_or_default();
            if !footprint.reaches(p[0], p[2]) {
                continue;
            }
            let edge = match fade {
                Some(f) => f.edge(p[0], p[2]) * footprint.vertical_ramp(p[1], layer.blend_distance),
                None => footprint.edge(p[0], p[1], p[2], layer.blend_distance),
            };
            let w0 = layer.clamped_weight() * edge;
            if w0 <= 0.0 {
                continue;
            }
            let scales = component_scales(&layer.volume, settings, lights);
            let ground = sun_relight(&layer.volume, settings, lights).ground_factor();
            let Some(g) = layer_lookup(&layer.volume, settings, &scales, ground, p, n, v) else {
                continue;
            };
            let w = w0 * g.cov;
            if w <= 0.0 {
                continue;
            }
            add_scaled(&mut tier_sh, &g.sh, w);
            tier_w += w;
            tier_vis += w * g.vis;
            tier_split.accumulate(&g.split, w);
        }
        if tier_w > 0.0 {
            let take = (1.0 - acc) * tier_w.min(1.0);
            add_scaled(&mut out, &tier_sh, take / tier_w);
            vis += take * tier_vis / tier_w;
            split.accumulate(&tier_split.finish(1.0 / tier_w), take);
            acc += take;
        }
    }
    if acc <= 1e-5 {
        return StaticGiSample::default();
    }
    let inv = 1.0 / acc;
    let mut total = [[0.0f32; 4]; 3];
    let mut components = [[0.0f32; 3]; GI_COMPONENTS];
    for (c, comp) in out.iter().enumerate() {
        let mut scaled = *comp;
        for (ch, t) in scaled.iter_mut().zip(total.iter_mut()) {
            for (m, tm) in ch.iter_mut().zip(t.iter_mut()) {
                *m *= inv;
                *tm += *m;
            }
        }
        components[c] = GiEval::of(settings).irradiance(&scaled, n);
    }
    StaticGiSample {
        irradiance_over_pi: GiEval::of(settings).irradiance(&total, n),
        components,
        coverage: acc,
        visibility: vis * inv,
        ..StaticGiSample::from_split(acc, &split.finish(inv))
    }
}

fn add_scaled(dst: &mut [ShRgb; GI_COMPONENTS], src: &[ShRgb; GI_COMPONENTS], k: f32) {
    for (d, s) in dst.iter_mut().zip(src) {
        for (dc, sc) in d.iter_mut().zip(s) {
            for (dm, sm) in dc.iter_mut().zip(sc) {
                *dm += sm * k;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_asset::baked_gi::{
        BakeInfo, BakeLights, BrickFlags, DetailBrick, ProbeData, ProbeFlags, ProbeMeta, SH_Y00,
        TerrainLayer, VolumeLayout,
    };

    const UP: [f32; 3] = [0.0, 1.0, 0.0];

    /// Radiance SH of a uniform radiance `l` (no L1).
    fn uniform(l: [f32; 3]) -> ShRgb {
        let k = 4.0 * std::f32::consts::PI * SH_Y00;
        [
            [l[0] * k, 0.0, 0.0, 0.0],
            [l[1] * k, 0.0, 0.0, 0.0],
            [l[2] * k, 0.0, 0.0, 0.0],
        ]
    }

    fn probe(sky: [f32; 3]) -> ProbeData {
        ProbeData {
            sh: [
                uniform([0.0; 3]),
                uniform(sky),
                uniform([0.0; 3]),
                uniform([0.0; 3]),
            ],
            moments: vec![[16.0, 256.0]; 16],
            meta: ProbeMeta {
                offset: [0.0; 3],
                flags: ProbeFlags::VALID,
            },
        }
    }

    fn lights() -> BakeLights {
        BakeLights {
            sky_scale: 1.0,
            ..BakeLights::default()
        }
    }

    /// A flat terrain tile (ground 0, levels 1, 2, 4 m, 4 m columns) with column (0, 0) at
    /// world `(x0, z0)` and `counts` columns, its sky light a function of the probe's world X, Z.
    fn tile_at(x0: f32, z0: f32, counts: [u32; 2], sky: impl Fn(f32, f32) -> f32) -> Arc<BakedGiVolume> {
        let t = TerrainLayer {
            origin: [x0, z0],
            spacing: [4.0, 4.0],
            counts,
            levels: vec![1.0, 2.0, 4.0],
            ground: vec![0.0; (counts[0] * counts[1]) as usize],
        };
        let layout = VolumeLayout {
            terrain: Some(t.clone()),
            bricks: Vec::new(),
            moment_resolution: 4,
            max_distance: 16.0,
        };
        let probes: Vec<_> = (0..layout.probe_count())
            .map(|i| {
                let (ci, cj, _) = t.coords(i);
                let l = sky(x0 + ci as f32 * 4.0, z0 + cj as f32 * 4.0);
                probe([l, 0.8 * l, 0.6 * l])
            })
            .collect();
        Arc::new(BakedGiVolume::from_probes(&layout, lights(), BakeInfo::default(), &probes).expect("tile"))
    }

    /// [`tile_at`] at z = 0, four columns deep (z = 0..12), lit as a function of X.
    fn tile(x0: f32, columns: u32, sky: impl Fn(f32) -> f32) -> Arc<BakedGiVolume> {
        tile_at(x0, 0.0, [columns, 4], |x, _| sky(x))
    }

    fn settings() -> BakedGiSettings {
        BakedGiSettings {
            normal_bias: 0.0,
            view_bias: 0.0,
            ..BakedGiSettings::default()
        }
    }

    fn frame() -> BakedGiFrameLights {
        BakedGiFrameLights::default()
    }

    /// The blend at `(x, 1.5, z)`, facing up.
    fn at_xz(layers: &[BakedGiLayer], x: f32, z: f32) -> StaticGiSample {
        sample_static_gi_blend(layers, &settings(), &frame(), [x, 1.5, z], UP, UP)
    }

    fn at(layers: &[BakedGiLayer], x: f32) -> StaticGiSample {
        at_xz(layers, x, 6.0)
    }

    /// The red irradiance of `volume` read alone at `(x, 1.5, z)`, as stored: the volume keeps
    /// L0 in RGB9E5, so a tile authored at 1.0 reads 1.00056. Blends are held to these readings,
    /// not to the authored constants.
    fn alone(volume: &Arc<BakedGiVolume>, x: f32, z: f32) -> f32 {
        at_xz(&[BakedGiLayer::new(Arc::clone(volume))], x, z).irradiance_over_pi[0]
    }

    /// The largest change of the red irradiance and of the coverage between neighboring
    /// samples 5 cm apart along X over `x0..=x1` at `z` (a jump at a seam shows as one large
    /// step).
    fn largest_steps(layers: &[BakedGiLayer], x0: f32, x1: f32, z: f32) -> (f32, f32) {
        let mut prev = at_xz(layers, x0, z);
        let (mut de, mut dc) = (0.0f32, 0.0f32);
        let steps = ((x1 - x0) / 0.05).round() as u32;
        for i in 1..=steps {
            let s = at_xz(layers, x0 + i as f32 * 0.05, z);
            de = de.max((s.irradiance_over_pi[0] - prev.irradiance_over_pi[0]).abs());
            dc = dc.max((s.coverage - prev.coverage).abs());
            prev = s;
        }
        (de, dc)
    }

    fn ramp(x: f32) -> f32 {
        1.0 + 0.05 * x
    }

    #[test]
    fn one_layer_is_the_single_volume_lookup_bit_for_bit() {
        let vol = tile(0.0, 4, ramp);
        let layers = [BakedGiLayer::new(vol.clone())];
        let s = settings();
        let scales = component_scales(&vol, &s, &frame());
        for x in [-3.0f32, -1.0, 0.0, 2.5, 6.0, 11.9, 13.0] {
            let p = [x, 1.5, 6.0];
            let a = sample_static_gi_blend(&layers, &s, &frame(), p, UP, UP);
            let b = super::super::sample_static_gi(&vol, &s, &scales, p, UP, UP);
            assert_eq!(a, b, "{x}");
        }
        // The general path agrees with it to rounding: a half-weight layer reads the same light
        // at half the coverage.
        let half = [BakedGiLayer {
            weight: 0.5,
            ..BakedGiLayer::new(vol.clone())
        }];
        let p = [6.0, 1.5, 6.0];
        let a = sample_static_gi_blend(&half, &s, &frame(), p, UP, UP);
        let b = super::super::sample_static_gi(&vol, &s, &scales, p, UP, UP);
        assert!((a.coverage - 0.5 * b.coverage).abs() < 1e-6, "{a:?} {b:?}");
        for c in 0..3 {
            assert!(
                (a.irradiance_over_pi[c] - b.irradiance_over_pi[c]).abs() < 1e-5 * b.irradiance_over_pi[c],
                "{a:?} {b:?}"
            );
        }
    }

    /// Two tiles of one continuous field, the second starting one column past the first's last
    /// (the usual way a streamed world is cut): across the seam cell the blend is the light a
    /// single volume over both gives, at full coverage, with no step.
    #[test]
    fn abutting_tiles_read_as_one_volume_across_the_seam() {
        let a = tile(0.0, 4, ramp); // columns at x = 0..12
        let b = tile(16.0, 4, ramp); // columns at x = 16..28
        let whole = tile(0.0, 8, ramp); // x = 0..28
        let tiles = [BakedGiLayer::new(a), BakedGiLayer::new(b)];
        let one = [BakedGiLayer::new(whole)];
        for i in 0..=112 {
            let x = i as f32 * 0.25;
            let t = at(&tiles, x);
            let w = at(&one, x);
            assert!((t.coverage - 1.0).abs() < 1e-5, "coverage dips at {x}: {t:?}");
            // In the seam cell the tiles interpolate their edge columns linearly; the single
            // volume weights the same two columns by DDGI's direction term too, and each tile
            // keeps a 0.001 floor on its clamped neighbor column: a few thousandths of ~1.7.
            for c in 0..3 {
                assert!(
                    (t.irradiance_over_pi[c] - w.irradiance_over_pi[c]).abs() < 0.004,
                    "x {x}: tiles {t:?} whole {w:?}"
                );
            }
        }
        let (de, dc) = largest_steps(&tiles, 0.0, 28.0, 6.0);
        // The field's own slope is 0.05 per metre: 0.0025 per 5 cm step.
        assert!(de < 0.004, "largest light step {de}");
        assert!(dc < 1e-5, "largest coverage step {dc}");
    }

    /// Two tiles baked apart that disagree at the seam: the light moves from one to the other
    /// continuously, the coverage stays full, and each tile reads alone away from the seam.
    #[test]
    fn tiles_that_disagree_meet_without_a_step() {
        let a = tile(0.0, 4, |_| 1.0);
        let b = tile(16.0, 4, |_| 3.0);
        let (la, lb) = (alone(&a, 6.0, 6.0), alone(&b, 22.0, 6.0));
        assert!((la - 1.0).abs() < 0.01 && (lb - 3.0).abs() < 0.03);
        let tiles = [BakedGiLayer::new(a), BakedGiLayer::new(b)];
        assert!((at(&tiles, 6.0).irradiance_over_pi[0] - la).abs() < 1e-4);
        assert!((at(&tiles, 22.0).irradiance_over_pi[0] - lb).abs() < 1e-4);
        let mid = at(&tiles, 14.0);
        assert!(
            (mid.irradiance_over_pi[0] - 0.5 * (la + lb)).abs() < 1e-3,
            "{mid:?}"
        );
        assert!((mid.coverage - 1.0).abs() < 1e-5);
        // The difference of 2 spread linearly over the 4 m seam cell: 0.025 per 5 cm.
        let (de, dc) = largest_steps(&tiles, 0.0, 28.0, 6.0);
        assert!(de < 0.026, "largest light step {de}");
        assert!(dc < 1e-5, "largest coverage step {dc}");
    }

    /// Tiles that overlap by 8 m with an 8 m blend distance: the weights are a partition of
    /// unity over the overlap, so the light moves linearly across all of it (half the steepest
    /// slope of the same tiles without a blend distance) and the coverage never dips.
    #[test]
    fn a_blend_distance_spreads_the_hand_over_across_the_overlap() {
        // x = 0..36 and 28..64, z = 0..28; sampled at z = 14, at least 8 m from every outer edge.
        let a = tile_at(0.0, 0.0, [10, 8], |_, _| 1.0);
        let b = tile_at(28.0, 0.0, [10, 8], |_, _| 3.0);
        let with = |blend: f32| {
            [
                BakedGiLayer {
                    blend_distance: blend,
                    ..BakedGiLayer::new(a.clone())
                },
                BakedGiLayer {
                    blend_distance: blend,
                    ..BakedGiLayer::new(b.clone())
                },
            ]
        };
        let wide = with(8.0);
        let (de, dc) = largest_steps(&wide, 10.0, 54.0, 14.0);
        let (de_none, _) = largest_steps(&with(0.0), 10.0, 54.0, 14.0);
        // 2 over 8 m: 0.0125 per 5 cm.
        assert!(de < 0.0126, "largest light step with the blend distance {de}");
        assert!(
            de < 0.6 * de_none,
            "the blend distance spreads the hand-over: {de} vs {de_none}"
        );
        assert!(dc < 1e-5, "the coverage stays full: {dc}");
        let (la, lb) = (alone(&a, 27.0, 14.0), alone(&b, 37.0, 14.0));
        assert!((at_xz(&wide, 32.0, 14.0).irradiance_over_pi[0] - 0.5 * (la + lb)).abs() < 1e-3);
        assert!((at_xz(&wide, 27.0, 14.0).irradiance_over_pi[0] - la).abs() < 1e-4);
        assert!((at_xz(&wide, 37.0, 14.0).irradiance_over_pi[0] - lb).abs() < 1e-4);
    }

    #[test]
    fn the_layer_split_blends_with_the_light() {
        // Two terrain tiles with 4 m and 8 m columns overlapping on x = 28..36 with an 8 m blend
        // distance (ADR-1104): the terrain layer holds all the light everywhere, and its cell
        // size is each tile's own away from the overlap and the light-weighted mean inside it.
        let spaced = |x0: f32, spacing: f32, counts: [u32; 2]| {
            let t = TerrainLayer {
                origin: [x0, 0.0],
                spacing: [spacing, spacing],
                counts,
                levels: vec![1.0, 2.0, 4.0],
                ground: vec![0.0; (counts[0] * counts[1]) as usize],
            };
            let layout = VolumeLayout {
                terrain: Some(t),
                bricks: Vec::new(),
                moment_resolution: 4,
                max_distance: 16.0,
            };
            let probes: Vec<_> = (0..layout.probe_count()).map(|_| probe([1.0; 3])).collect();
            Arc::new(
                BakedGiVolume::from_probes(&layout, lights(), BakeInfo::default(), &probes).expect("tile"),
            )
        };
        let layers = [
            BakedGiLayer {
                blend_distance: 8.0,
                ..BakedGiLayer::new(spaced(0.0, 4.0, [10, 8]))
            },
            BakedGiLayer {
                blend_distance: 8.0,
                ..BakedGiLayer::new(spaced(28.0, 8.0, [6, 4]))
            },
        ];
        for (x, cell) in [(16.0, 4.0), (32.0, 6.0), (50.0, 8.0)] {
            let s = at_xz(&layers, x, 14.0);
            assert!(s.coverage > 0.99, "x = {x}: {s:?}");
            assert!((s.terrain_share - s.coverage).abs() < 1e-5, "x = {x}: {s:?}");
            assert_eq!((s.brick_share, s.brick_cell), (0.0, 0.0), "x = {x}: no brick");
            assert!((s.terrain_cell - cell).abs() < 1e-4, "x = {x}: {s:?}");
        }
    }

    /// A brick box one probe layer tall still weighs its middle in with a blend distance: it is
    /// open in Y, as its Y ramp would be 0 everywhere (corr-low-render, corr-render-rest finding
    /// 15). A two-layer box stays closed.
    #[test]
    fn a_single_layer_brick_box_is_not_weighed_out_by_the_y_ramp() {
        let sheet = |counts: [u32; 3]| {
            let mut layout = VolumeLayout {
                terrain: None,
                bricks: vec![DetailBrick {
                    origin: [0.0; 3],
                    spacing: [1.0; 3],
                    counts,
                    first_probe: 0,
                    flags: BrickFlags::INTERIOR,
                }],
                moment_resolution: 4,
                max_distance: 16.0,
            };
            layout.assign_brick_offsets();
            let probes: Vec<_> = (0..layout.probe_count()).map(|_| probe([1.0; 3])).collect();
            BakedGiVolume::from_probes(&layout, lights(), BakeInfo::default(), &probes).expect("sheet")
        };
        let flat = BakedGiFootprint::of(&sheet([4, 1, 4])).expect("footprint");
        let w = flat.edge(1.5, 0.0, 1.5, 0.25);
        assert!(w > 0.0, "the middle of a one-layer sheet weighs {w}");
        assert!(!flat.y_closed);
        let two = BakedGiFootprint::of(&sheet([4, 2, 4])).expect("footprint");
        assert!(two.y_closed);
        assert_eq!(two.edge(1.5, 0.5, 1.5, 0.25), 1.0);
    }

    fn cube(origin: [f32; 3], sky: f32) -> Arc<BakedGiVolume> {
        let mut layout = VolumeLayout {
            terrain: None,
            bricks: vec![DetailBrick {
                origin,
                spacing: [1.0; 3],
                counts: [2, 2, 2],
                first_probe: 0,
                flags: BrickFlags::INTERIOR,
            }],
            moment_resolution: 4,
            max_distance: 16.0,
        };
        layout.assign_brick_offsets();
        let probes: Vec<_> = (0..layout.probe_count())
            .map(|_| probe([sky, 0.8 * sky, 0.6 * sky]))
            .collect();
        Arc::new(BakedGiVolume::from_probes(&layout, lights(), BakeInfo::default(), &probes).expect("cube"))
    }

    /// Two unit-cube brick volumes (probe box 1 m on a side) overlap by `o` with
    /// `blend_distance = o`. Through the slab the edge weights are the linear partition of
    /// unity; outside it they equal the one volume that contains the point. The same cubes
    /// overlapped in X do the same. A blend distance of 0 does not ramp, including in Y, and a
    /// point outside both cubes has coverage 0.
    #[test]
    fn stacked_unit_cubes_blend_across_y_and_the_same_fixture_blends_across_x() {
        let o = 0.25f32;
        let pair = |origin_b: [f32; 3], blend: f32| {
            let a = cube([0.0; 3], 1.0);
            let b = cube(origin_b, 3.0);
            let layers = [
                BakedGiLayer {
                    blend_distance: blend,
                    ..BakedGiLayer::new(a.clone())
                },
                BakedGiLayer {
                    blend_distance: blend,
                    ..BakedGiLayer::new(b.clone())
                },
            ];
            (a, b, layers)
        };
        // Fraction `t` across an overlap of width `o` whose faces sit at `face_a` (A's far
        // face) and `face_b` (B's near face). Axes other than the overlap axis are the cube
        // centres, so their ramps are 1.
        let check = |origin_b: [f32; 3], axis: usize, normal: [f32; 3]| {
            let (a, b, wide) = pair(origin_b, o);
            let fa = BakedGiFootprint::of(&a).expect("footprint");
            let fb = BakedGiFootprint::of(&b).expect("footprint");
            assert!(fa.y_closed && fb.y_closed);
            assert_eq!(fa.max[0] - fa.min[0], 1.0);
            assert_eq!(fa.max_y - fa.min_y, 1.0);
            assert_eq!(fa.reach_y, 0.5);
            let at = |p: [f32; 3]| sample_static_gi_blend(&wide, &settings(), &frame(), p, normal, normal);
            let lone = |volume: &Arc<BakedGiVolume>, p: [f32; 3]| {
                sample_static_gi_blend(
                    &[BakedGiLayer::new(Arc::clone(volume))],
                    &settings(),
                    &frame(),
                    p,
                    normal,
                    normal,
                )
            };
            // Blend distance 0 is a hard cut: the factor is 1 on the face and outside the box.
            assert_eq!(fa.edge(0.5, 1.0, 0.5, 0.0), 1.0);
            assert_eq!(fa.edge(0.5, 2.0, 0.5, 0.0), 1.0);
            for (t, along) in [
                (0.0, 0.75),
                (0.25, 0.8125),
                (0.5, 0.875),
                (0.75, 0.9375),
                (1.0, 1.0),
            ] {
                let mut p = [0.5; 3];
                p[axis] = along;
                let (wa, wb) = (fa.edge(p[0], p[1], p[2], o), fb.edge(p[0], p[1], p[2], o));
                assert!((wa - (1.0 - t)).abs() < 1e-5, "axis {axis} t {t}: {wa}");
                assert!((wb - t).abs() < 1e-5, "axis {axis} t {t}: {wb}");
                assert!((wa + wb - 1.0).abs() < 1e-5, "axis {axis} t {t}");
                let s = at(p);
                let (la, lb) = (lone(&a, p), lone(&b, p));
                let light = (1.0 - t) * la.irradiance_over_pi[0] + t * lb.irradiance_over_pi[0];
                assert!(
                    (s.irradiance_over_pi[0] - light).abs() < 1e-3,
                    "axis {axis} t {t}: {s:?} vs {light} ({la:?} {lb:?})"
                );
                assert!((s.coverage - 1.0).abs() < 1e-4, "axis {axis} t {t}: {s:?}");
            }
            // Outside the slab the other volume's weight is 0, so the sum is the one volume.
            let mut inside = [0.5; 3];
            inside[axis] = 0.5;
            let (wa, wb) = (
                fa.edge(inside[0], inside[1], inside[2], o),
                fb.edge(inside[0], inside[1], inside[2], o),
            );
            assert!((wa - 1.0).abs() < 1e-5 && wb.abs() < 1e-6, "{wa} {wb}");
            let s = at(inside);
            let la = lone(&a, inside);
            assert!((s.irradiance_over_pi[0] - la.irradiance_over_pi[0]).abs() < 1e-4);
            assert!((s.coverage - la.coverage).abs() < 1e-5);
            // Outside the slab and near a side face, X and Y both ramp and multiply.
            if axis == 1 {
                let p = [0.125, 0.125, 0.5];
                let wa = fa.edge(p[0], p[1], p[2], o);
                assert!((wa - 0.25).abs() < 1e-5, "X and Y multiply: {wa}");
                assert_eq!(fb.edge(p[0], p[1], p[2], o), 0.0);
                let s = at(p);
                assert!((s.coverage - wa * lone(&a, p).coverage).abs() < 1e-4, "{s:?}");
            }
            // Blend distance 0 does not ramp across the slab: both volumes stay at full weight,
            // so the light is the even mix, not the 3:1 mix the ramp gives at t = 0.25.
            let (_, _, hard) = pair(origin_b, 0.0);
            let mut p = [0.5; 3];
            p[axis] = 0.8125;
            let hard_s = sample_static_gi_blend(&hard, &settings(), &frame(), p, normal, normal);
            let (la, lb) = (lone(&a, p), lone(&b, p));
            let even = 0.5 * (la.irradiance_over_pi[0] + lb.irradiance_over_pi[0]);
            let ramped = 0.75 * la.irradiance_over_pi[0] + 0.25 * lb.irradiance_over_pi[0];
            assert!(
                (hard_s.irradiance_over_pi[0] - even).abs() < 1e-3,
                "{hard_s:?} vs {even}"
            );
            assert!(
                (hard_s.irradiance_over_pi[0] - ramped).abs() > 0.2,
                "the hard cut is not the ramp"
            );
        };
        // Stacked in Y. A horizontal normal keeps the interior test off the Y faces.
        check([0.0, 0.75, 0.0], 1, [0.0, 0.0, 1.0]);
        // The same fixture overlapped in X.
        check([0.75, 0.0, 0.0], 0, [0.0, 1.0, 0.0]);

        let (_, _, wide) = pair([0.0, 0.75, 0.0], o);
        let n = [0.0, 0.0, 1.0];
        for p in [[0.5, -2.0, 0.5], [0.5, 5.0, 0.5], [4.0, 0.5, 0.5]] {
            let s = sample_static_gi_blend(&wide, &settings(), &frame(), p, n, n);
            assert_eq!(s.coverage, 0.0, "outside every volume at {p:?}: {s:?}");
        }
        let (_, _, hard) = pair([0.0, 0.75, 0.0], 0.0);
        let s = sample_static_gi_blend(&hard, &settings(), &frame(), [0.5, -2.0, 0.5], n, n);
        assert_eq!(s.coverage, 0.0, "blend distance 0 is still empty outside: {s:?}");

        // The GPU uniform carries the closed Y span (the shader multiplies the same ramp).
        use super::super::{VolumeWords, set_uniform};
        let (ordered, dropped) = order_layers(&wide);
        assert_eq!(dropped, 0);
        let mut end = 0usize;
        let words: Vec<_> = ordered
            .iter()
            .map(|l| {
                let w = VolumeWords::of(&l.volume).shifted(end);
                end = w.end;
                w
            })
            .collect();
        let set = set_uniform(&ordered, &words, &settings(), &frame());
        assert_eq!(set.layers[0].vertical, [0.0, 1.0, 0.5, 1.0]);
        assert_eq!(set.layers[1].vertical, [0.75, 1.75, 0.5, 1.0]);
    }

    /// Execute the production lookup and blend, with analytic edge weights and CPU light as
    /// independent oracles. No screen reconstruction, timing budget or reference image.
    #[test]
    fn stacked_cubes_shader_matches_analytic_weights_and_cpu_light() {
        use super::super::{VolumeWords, read_words, screen_source, set_uniform, write_volume};
        use wgpu::util::DeviceExt;

        let device = crate::device::test_device();
        let d = &device.device;
        let source = format!(
            "{}\n{}",
            screen_source(),
            r#"
@group(2) @binding(0) var<storage, read> test_points: array<vec4<f32>>;
@group(2) @binding(1) var<storage, read_write> test_results: array<vec4<f32>>;
@compute @workgroup_size(1)
fn test_blend(@builtin(global_invocation_id) gid: vec3<u32>) {
    let p = test_points[gid.x].xyz;
    let n = vec3<f32>(0.0, 0.0, 1.0);
    let s = static_gi_set(p, n, n);
    test_results[gid.x] = vec4<f32>(bgi_irradiance_over_pi(s.sh, n).x, s.cov,
                                   bgi_layer_edge(0u, p), bgi_layer_edge(1u, p));
}
"#
        );
        let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("stacked GI cubes"),
            source: wgpu::ShaderSource::Wgsl(source.into()),
        });
        let pipeline = d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("stacked GI cubes"),
            layout: None,
            module: &module,
            entry_point: Some("test_blend"),
            compilation_options: Default::default(),
            cache: None,
        });
        let buffer = |label, contents: &[u8], usage| {
            d.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some(label),
                contents,
                usage,
            })
        };
        for axis in [1, 0] {
            for blend in [0.25, 0.0] {
                let mut origin = [0.0; 3];
                origin[axis] = 0.75;
                let layers = [cube([0.0; 3], 1.0), cube(origin, 3.0)].map(|volume| BakedGiLayer {
                    blend_distance: blend,
                    ..BakedGiLayer::new(volume)
                });
                let (ordered, _) = order_layers(&layers);
                let mut end = 0;
                let words: Vec<_> = ordered
                    .iter()
                    .map(|l| {
                        let w = VolumeWords::of(&l.volume).shifted(end);
                        end = w.end;
                        w
                    })
                    .collect();
                let mut bytes = vec![0; end * 4];
                for (l, w) in ordered.iter().zip(&words) {
                    write_volume(&l.volume, w, |at, b| bytes[at..at + b.len()].copy_from_slice(b));
                }
                let settings = settings();
                let lights = frame();
                let set = set_uniform(&ordered, &words, &settings, &lights);
                let uniform = buffer("GI set", bytemuck::bytes_of(&set), wgpu::BufferUsages::UNIFORM);
                let data = buffer("GI cubes", &bytes, wgpu::BufferUsages::STORAGE);
                let core = d.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("GI cubes"),
                    layout: &pipeline.get_bind_group_layout(0),
                    entries: &[2, 3, 5, 6, 7].map(|binding| wgpu::BindGroupEntry {
                        binding,
                        resource: if binding == 2 {
                            uniform.as_entire_binding()
                        } else {
                            data.as_entire_binding()
                        },
                    }),
                });
                let empty = d.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("unused screen inputs"),
                    layout: &pipeline.get_bind_group_layout(1),
                    entries: &[],
                });
                // Single-volume samples (including partial edge weights), the overlap, and empty space.
                let positions: Vec<[f32; 4]> = [
                    -2.0, 0.125, 0.5, 0.75, 0.8125, 0.875, 0.9375, 1.0, 1.25, 1.625, 4.0,
                ]
                .into_iter()
                .map(|q| {
                    let mut p = [0.5, 0.5, 0.5, 0.0];
                    p[axis] = q;
                    p
                })
                .collect();
                let points = buffer(
                    "GI points",
                    bytemuck::cast_slice(&positions),
                    wgpu::BufferUsages::STORAGE,
                );
                let results = buffer(
                    "GI results",
                    &vec![0; positions.len() * 16],
                    wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
                );
                let inputs = d.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("GI points and results"),
                    layout: &pipeline.get_bind_group_layout(2),
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: points.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: results.as_entire_binding(),
                        },
                    ],
                });
                let mut encoder = d.create_command_encoder(&Default::default());
                {
                    let mut pass = encoder.begin_compute_pass(&Default::default());
                    pass.set_pipeline(&pipeline);
                    pass.set_bind_group(0, &core, &[]);
                    pass.set_bind_group(1, &empty, &[]);
                    pass.set_bind_group(2, &inputs, &[]);
                    pass.dispatch_workgroups(positions.len() as u32, 1, 1);
                }
                device.queue.submit([encoder.finish()]);
                let output =
                    read_words(&device, &results, 0, (positions.len() * 16) as u64).expect("GPU readback");
                for (p, words) in positions.iter().zip(output.as_chunks::<4>().0) {
                    let gpu = words.map(f32::from_bits);
                    let p3 = [p[0], p[1], p[2]];
                    let n = [0.0, 0.0, 1.0];
                    let cpu = sample_static_gi_blend(&layers, &settings, &lights, p3, n, n);
                    assert!(
                        (gpu[0] - cpu.irradiance_over_pi[0]).abs() < 0.002,
                        "axis {axis}, blend {blend}, {p:?}: {gpu:?} vs {cpu:?}"
                    );
                    assert!((gpu[1] - cpu.coverage).abs() < 1e-5, "{p:?}: {gpu:?} vs {cpu:?}");
                    let q = p[axis];
                    let expected = if blend == 0.0 {
                        [1.0, 1.0]
                    } else {
                        [
                            ((q.min(1.0 - q)) / blend).clamp(0.0, 1.0),
                            (((q - 0.75).min(1.75 - q)) / blend).clamp(0.0, 1.0),
                        ]
                    };
                    assert!(
                        (gpu[2] - expected[0]).abs() < 1e-6 && (gpu[3] - expected[1]).abs() < 1e-6,
                        "axis {axis}, blend {blend}, {p:?}: {gpu:?} vs {expected:?}"
                    );
                    if blend > 0.0 {
                        assert!((gpu[1] - (expected[0] + expected[1]).min(1.0)).abs() < 1e-5);
                    }
                    if (0.75..=1.0).contains(&q) && blend > 0.0 {
                        assert!((gpu[2] + gpu[3] - 1.0).abs() < 1e-6);
                    }
                    if !(-1.0..=3.0).contains(&q) {
                        assert_eq!(gpu[1], 0.0);
                    }
                }
            }
        }
    }

    /// Four tiles meeting at a corner, overlapping by 8 m with an 8 m blend distance: full
    /// coverage everywhere around the corner and no step along either axis or the diagonal.
    #[test]
    fn four_tiles_meet_at_a_corner_without_a_hole_or_a_step() {
        let mut layers = Vec::new();
        for (i, (x0, z0)) in [(0.0, 0.0), (28.0, 0.0), (0.0, 28.0), (28.0, 28.0)]
            .into_iter()
            .enumerate()
        {
            let l = 1.0 + i as f32;
            layers.push(BakedGiLayer {
                blend_distance: 8.0,
                ..BakedGiLayer::new(tile_at(x0, z0, [10, 10], move |_, _| l))
            });
        }
        let mut prev_diag: Option<f32> = None;
        for i in 0..=88 {
            let q = 10.0 + i as f32 * 0.5;
            for z in [14.0, 30.0, 32.0, 34.0, 50.0] {
                let s = at_xz(&layers, q, z);
                assert!((s.coverage - 1.0).abs() < 1e-5, "hole at ({q}, {z}): {s:?}");
            }
            let d = at_xz(&layers, q, q).irradiance_over_pi[0];
            if let Some(p) = prev_diag {
                assert!((d - p).abs() < 0.2, "step on the diagonal at {q}: {p} → {d}");
            }
            prev_diag = Some(d);
        }
        // At the corner's centre all four share equally.
        let c = at_xz(&layers, 32.0, 32.0);
        assert!((c.irradiance_over_pi[0] - 2.5).abs() < 1e-3, "{c:?}");
        for z in [14.0, 32.0, 50.0] {
            let (de, dc) = largest_steps(&layers, 10.0, 54.0, z);
            assert!(de < 0.026, "largest light step at z = {z}: {de}");
            assert!(dc < 1e-5, "coverage step at z = {z}: {dc}");
        }
    }

    /// Tiles that leave a gap wider than their fringe: the coverage falls to 0 between them
    /// continuously (the frame's other GI fills it), never as a step.
    #[test]
    fn a_gap_between_tiles_fades_out_and_back_in() {
        let a = tile(0.0, 4, |_| 1.0); // ends at 12, covers to 16
        let b = tile(24.0, 4, |_| 1.0); // starts at 24, covers from 20
        let tiles = [BakedGiLayer::new(a), BakedGiLayer::new(b)];
        assert_eq!(at(&tiles, 18.0).coverage, 0.0);
        assert!((at(&tiles, 14.0).coverage - 0.5).abs() < 1e-4);
        let (_, dc) = largest_steps(&tiles, 0.0, 36.0, 6.0);
        assert!(dc < 0.0126, "the coverage moves 1/4 per metre at most: {dc}");
    }

    /// A small high-priority volume inside a large one: it lights its inside alone and hands
    /// back to the large one over its blend distance; the coverage stays full.
    #[test]
    fn a_higher_priority_volume_nested_inside_hands_over_smoothly() {
        let world = tile(-16.0, 12, |_| 1.0); // x = -16..28
        let set_piece = tile(4.0, 3, |_| 4.0); // x = 4..12
        let (inner, outer) = (alone(&set_piece, 8.0, 6.0), alone(&world, -6.0, 6.0));
        assert!((inner - 4.0).abs() < 0.01 && (outer - 1.0).abs() < 0.01);
        let layers = [
            BakedGiLayer::new(world),
            BakedGiLayer {
                priority: 1,
                blend_distance: 4.0,
                ..BakedGiLayer::new(set_piece)
            },
        ];
        assert!((at(&layers, 8.0).irradiance_over_pi[0] - inner).abs() < 1e-4);
        assert!((at(&layers, 6.0).irradiance_over_pi[0] - 0.5 * (inner + outer)).abs() < 1e-3);
        assert!((at(&layers, -6.0).irradiance_over_pi[0] - outer).abs() < 1e-4);
        assert!(
            (at(&layers, 3.0).irradiance_over_pi[0] - outer).abs() < 1e-4,
            "no fringe outside"
        );
        // 3 over the 4 m hand-over: 0.0375 per 5 cm.
        let (de, dc) = largest_steps(&layers, -10.0, 24.0, 6.0);
        assert!(de < 0.038, "largest light step {de}");
        assert!(dc < 1e-5, "the coverage stays full: {dc}");
        // The priority decides, not the order the host listed them in.
        let swapped = [layers[1].clone(), layers[0].clone()];
        for x in [-6.0, 3.0, 5.0, 8.0, 15.0] {
            assert_eq!(at(&layers, x), at(&swapped, x), "{x}");
        }
    }

    /// A streamed tile fading in: its share grows with its weight, continuously, from nothing.
    #[test]
    fn a_fading_layer_moves_the_light_continuously() {
        let world = tile(-16.0, 12, |_| 1.0);
        let streamed = tile(0.0, 4, |_| 3.0);
        let (base, full) = (alone(&world, 6.0, 6.0), alone(&streamed, 6.0, 6.0));
        let mut prev: Option<f32> = None;
        for step in 0..=20 {
            let weight = step as f32 / 20.0;
            let layers = [
                BakedGiLayer::new(world.clone()),
                BakedGiLayer {
                    weight,
                    ..BakedGiLayer::new(streamed.clone())
                },
            ];
            let e = at(&layers, 6.0).irradiance_over_pi[0];
            if step == 0 {
                assert!(
                    (e - base).abs() < 1e-4,
                    "a zero weight adds nothing: {e} vs {base}"
                );
            }
            if let Some(p) = prev {
                assert!(e >= p - 1e-5, "monotonic while fading in: {p} → {e}");
                assert!(e - p < 0.1 + 1e-4, "no jump: {p} → {e}");
            }
            prev = Some(e);
        }
        // Full weight at equal priority: an even blend where both cover fully.
        let even = 0.5 * (base + full);
        assert!(
            prev.is_some_and(|e| (e - even).abs() < 1e-3),
            "{prev:?} vs {even}"
        );
    }

    #[test]
    fn unusable_and_excess_layers_are_ignored_in_priority_order() {
        let v = tile(0.0, 4, |_| 1.0);
        let mut layers: Vec<BakedGiLayer> = (0..MAX_BAKED_GI_VOLUMES + 2)
            .map(|i| BakedGiLayer {
                priority: -(i as i32),
                ..BakedGiLayer::new(v.clone())
            })
            .collect();
        for bad in [
            BakedGiLayer {
                weight: f32::NAN,
                ..BakedGiLayer::new(v.clone())
            },
            BakedGiLayer {
                blend_distance: -1.0,
                ..BakedGiLayer::new(v.clone())
            },
            BakedGiLayer {
                weight: 0.0,
                ..BakedGiLayer::new(v.clone())
            },
        ] {
            assert!(!bad.usable());
            layers.push(bad);
        }
        let (ordered, dropped) = order_layers(&layers);
        assert_eq!(ordered.len(), MAX_BAKED_GI_VOLUMES);
        assert_eq!(dropped, 2);
        assert!(ordered.windows(2).all(|w| w[0].priority >= w[1].priority));
        assert_eq!(
            ordered[MAX_BAKED_GI_VOLUMES - 1].priority,
            -(MAX_BAKED_GI_VOLUMES as i32 - 1)
        );
        assert_eq!(at(&[], 6.0), StaticGiSample::default());
    }

    #[test]
    fn the_footprint_spans_terrain_and_bricks_and_the_edge_ramps_inside_it() {
        let v = tile(0.0, 4, |_| 1.0);
        let f = BakedGiFootprint::of(&v).expect("footprint");
        assert_eq!(f.min, [0.0, 0.0]);
        assert_eq!(f.max, [12.0, 12.0]);
        assert!(!f.y_closed, "a terrain layer is open in Y");
        assert_eq!(
            f.edge(-10.0, 1.5, 6.0, 0.0),
            1.0,
            "no blend distance: no edge weight"
        );
        assert_eq!(f.edge(-1.0, 1.5, 6.0, 2.0), 0.0);
        assert!((f.edge(1.0, 1.5, 6.0, 2.0) - 0.5).abs() < 1e-6);
        assert_eq!(f.edge(2.0, 1.5, 6.0, 2.0), 1.0);
        assert!(
            (f.edge(1.0, 1.5, 1.0, 2.0) - 0.25).abs() < 1e-6,
            "the axes multiply"
        );
        assert!(
            (f.edge(1.0, 100.0, 6.0, 2.0) - 0.5).abs() < 1e-6,
            "open Y does not cut a horizontal seam"
        );
        let mut layout = v.layout();
        layout.bricks.push(DetailBrick {
            origin: [10.0, 0.0, -3.0],
            spacing: [1.0; 3],
            counts: [6, 3, 4],
            first_probe: 0,
            flags: 0,
        });
        layout.assign_brick_offsets();
        let probes: Vec<_> = (0..layout.probe_count()).map(|_| probe([1.0; 3])).collect();
        let with_brick =
            BakedGiVolume::from_probes(&layout, lights(), BakeInfo::default(), &probes).expect("volume");
        let f = BakedGiFootprint::of(&with_brick).expect("footprint");
        assert!(!f.y_closed, "terrain keeps the volume open in Y");
        assert_eq!(f.min, [0.0, -3.0]);
        assert_eq!(f.max, [15.0, 12.0]);
        assert_eq!(
            f.reach,
            [4.0, 4.0],
            "the terrain fringe outreaches half a 1 m brick cell"
        );
        assert!(f.reaches(-3.9, 6.0) && !f.reaches(-4.1, 6.0));
        assert!(f.reaches(18.9, 15.9) && !f.reaches(19.1, 6.0));
    }

    /// Skipping a volume outside its footprint's reach changes nothing: there its own lookup
    /// covers nothing, for the terrain fringe, plain bricks and interior bricks (tested half a
    /// cell along the normal) alike.
    #[test]
    fn a_volume_covers_nothing_outside_its_reach() {
        // Terrain with a plain and an interior brick, and a volume of bricks only (2 m cells).
        let mut layout = tile(0.0, 4, |_| 1.0).layout();
        layout.bricks.push(DetailBrick {
            origin: [9.0, 0.0, 9.0],
            spacing: [1.0; 3],
            counts: [5, 3, 5],
            first_probe: 0,
            flags: 0,
        });
        layout.assign_brick_offsets();
        let probes: Vec<_> = (0..layout.probe_count()).map(|_| probe([1.0; 3])).collect();
        let mixed =
            BakedGiVolume::from_probes(&layout, lights(), BakeInfo::default(), &probes).expect("volume");
        let mut rooms = VolumeLayout {
            terrain: None,
            bricks: vec![DetailBrick {
                origin: [20.0, 0.0, 20.0],
                spacing: [2.0, 1.0, 2.0],
                counts: [4, 3, 3],
                first_probe: 0,
                flags: 0,
            }],
            moment_resolution: 4,
            max_distance: 16.0,
        };
        rooms.assign_brick_offsets();
        assert_eq!(
            rooms.flag_interior_bricks(&[([19.9, -0.1, 19.9], [26.1, 2.1, 24.1])]),
            1
        );
        let probes: Vec<_> = (0..rooms.probe_count()).map(|_| probe([1.0; 3])).collect();
        let rooms =
            BakedGiVolume::from_probes(&rooms, lights(), BakeInfo::default(), &probes).expect("volume");
        let s = settings();
        let normals = [
            [1.0, 0.0, 0.0],
            [-1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0],
            [0.0, 0.0, -1.0],
            [0.6, 0.0, 0.8],
            [0.0, 1.0, 0.0],
        ];
        let mut outside = 0;
        let mut covered_near_edge = 0;
        for vol in [&mixed, &rooms] {
            let f = BakedGiFootprint::of(vol).expect("footprint");
            let scales = component_scales(vol, &s, &frame());
            for i in 0..=80 {
                for j in 0..=80 {
                    let (x, z) = (-8.0 + i as f32 * 0.5, -8.0 + j as f32 * 0.5);
                    for n in normals {
                        let c = super::super::sample_static_gi(vol, &s, &scales, [x, 1.0, z], n, n).coverage;
                        if !f.reaches(x, z) {
                            assert_eq!(c, 0.0, "covered outside the reach at ({x}, {z}) {n:?}");
                            outside += 1;
                        } else if c > 0.0 && (x < f.min[0] || x > f.max[0] || z < f.min[1] || z > f.max[1]) {
                            covered_near_edge += 1;
                        }
                    }
                }
            }
        }
        assert!(outside > 10_000, "{outside}");
        assert!(
            covered_near_edge > 100,
            "the fringe and the interior push are exercised: {covered_near_edge}"
        );
    }

    fn struct_span(module: &naga::Module, name: &str) -> Option<u32> {
        module.types.iter().find_map(|(_, t)| match &t.inner {
            naga::TypeInner::Struct { span, .. } if t.name.as_deref() == Some(name) => Some(*span),
            _ => None,
        })
    }

    /// The Rust uniform has the shader's layout, and the tiers follow the priorities.
    #[test]
    fn the_set_uniform_matches_the_shader_layout_and_orders_tiers() {
        use super::super::{BakedGiLayerGpu, BakedGiSetGpu, BakedGiUniformGpu, VolumeWords, set_uniform};
        let src = super::super::screen_source();
        let module =
            naga::front::wgsl::parse_str(&src).unwrap_or_else(|e| panic!("{}", e.emit_to_string(&src)));
        assert_eq!(
            struct_span(&module, "BakedGiUniform"),
            Some(size_of::<BakedGiUniformGpu>() as u32)
        );
        assert_eq!(
            struct_span(&module, "BakedGiLayerGpu"),
            Some(size_of::<BakedGiLayerGpu>() as u32)
        );
        assert_eq!(
            struct_span(&module, "BakedGiSet"),
            Some(size_of::<BakedGiSetGpu>() as u32)
        );
        assert!(src.contains(&format!("const BGI_MAX_VOLUMES: u32 = {MAX_BAKED_GI_VOLUMES}u;")));
        assert!(src.contains(&format!("array<BakedGiUniform, {MAX_BAKED_GI_VOLUMES}>")));
        assert!(src.contains(&format!("array<BakedGiLayerGpu, {MAX_BAKED_GI_VOLUMES}>")));
        assert!(
            src.contains("vertical: vec4<f32>"),
            "the shader carries the Y span"
        );

        let v = |x0: f32| tile(x0, 4, |_| 1.0);
        let layers = [
            BakedGiLayer::new(v(0.0)),
            BakedGiLayer {
                priority: 2,
                blend_distance: 3.0,
                ..BakedGiLayer::new(v(16.0))
            },
            BakedGiLayer {
                priority: 2,
                weight: 0.25,
                ..BakedGiLayer::new(v(32.0))
            },
            BakedGiLayer {
                priority: -1,
                ..BakedGiLayer::new(v(48.0))
            },
        ];
        let (ordered, dropped) = order_layers(&layers);
        assert_eq!(dropped, 0);
        let mut end = 0;
        let words: Vec<VolumeWords> = ordered
            .iter()
            .map(|l| {
                let w = VolumeWords::of(&l.volume).shifted(end);
                end = w.end;
                w
            })
            .collect();
        let set = set_uniform(&ordered, &words, &settings(), &frame());
        assert_eq!(set.count[0], 4);
        assert_eq!(set.count[1], 0, "several volumes: the blend path");
        let tiers: Vec<u32> = set.layers[..4].iter().map(|l| l.tier[0]).collect();
        assert_eq!(tiers, [0, 0, 1, 2]);
        // Blend order: the two priority-2 layers in the host's order, then 0, then −1.
        assert_eq!(set.layers[0].rect, [16.0, 0.0, 28.0, 12.0]);
        assert_eq!(
            set.layers[0].fade,
            [3.0, 1.0, 4.0, 4.0],
            "blend, weight, reach x and z"
        );
        assert_eq!(set.layers[1].fade[..2], [0.0, 0.25]);
        assert_eq!(set.layers[2].rect, [0.0, 0.0, 12.0, 12.0]);
        assert_eq!(set.layers[0].vertical, [0.0; 4], "a terrain tile is open in Y");
        // Each volume's word offsets point at its own sections of the shared buffer.
        for (k, w) in words.iter().enumerate() {
            assert_eq!(
                set.volumes[k].words,
                [w.meta as u32, w.sh as u32, w.moments as u32, w.ground as u32]
            );
            assert_eq!(set.volumes[k].extra[0], w.bricks as u32);
        }
        assert!(words.windows(2).all(|p| p[1].meta == p[0].end));
        // One volume at full weight takes the unchanged single-volume path.
        let one = [BakedGiLayer::new(v(0.0))];
        let (o, _) = order_layers(&one);
        assert_eq!(
            set_uniform(&o, &words[..1], &settings(), &frame()).count,
            [1, 1, 0, 0]
        );
        let faded = [BakedGiLayer {
            weight: 0.5,
            ..BakedGiLayer::new(v(0.0))
        }];
        let (o, _) = order_layers(&faded);
        assert_eq!(
            set_uniform(&o, &words[..1], &settings(), &frame()).count,
            [1, 0, 0, 0]
        );
    }

    #[test]
    fn the_renderer_keeps_the_layers_and_hands_them_to_a_stand_in() {
        let a = tile(0.0, 4, |_| 1.0);
        let b = tile(16.0, 4, |_| 1.0);
        let mut r = crate::FrameRenderer::new();
        assert_eq!(r.set_baked_gi_layers(&[BakedGiLayer::new(b.clone())]), 1);
        let st = r.baked_gi_status();
        assert!(st.installed, "a layer alone lights the frame");
        assert_eq!(st.probes, b.probe_count());
        assert_eq!(st.volumes, 0, "no frame yet");
        r.set_baked_gi(Some(a.clone()));
        assert_eq!(r.set_baked_gi_layers(&[BakedGiLayer::new(b.clone())]), 2);
        assert_eq!(r.baked_gi_status().probes, a.probe_count() + b.probe_count());
        assert!(
            r.baked_gi().is_some_and(|v| Arc::ptr_eq(v, &a)),
            "the installed volume stays"
        );
        // A stand-in gets them; what it sets meanwhile comes back.
        let mut stand_in = crate::FrameRenderer::new();
        r.share_installs_with(&mut stand_in);
        assert_eq!(stand_in.baked_gi_layers().len(), 1);
        assert_eq!(stand_in.set_baked_gi_layers(&[]), 1);
        let mut back = crate::FrameRenderer::new();
        back.set_baked_gi_layers(&[BakedGiLayer::new(b)]);
        back.absorb_stand_in(&mut stand_in);
        assert!(back.baked_gi_layers().is_empty());
        // Only unusable layers: nothing is installed.
        let mut lone = crate::FrameRenderer::new();
        assert_eq!(
            lone.set_baked_gi_layers(&[BakedGiLayer {
                weight: 0.0,
                ..BakedGiLayer::new(a)
            }]),
            0
        );
        assert!(!lone.baked_gi_status().installed);
    }
}
