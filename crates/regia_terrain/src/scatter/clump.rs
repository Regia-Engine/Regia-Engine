// SPDX-License-Identifier: MPL-2.0
//! Cheap small stones (lane scatter-density): a coarse extra LOD by vertex clustering
//! ([`coarse_lod`]) and **clumps** ([`clump_asset`]), several pebbles merged into one scatter
//! mesh so a dense pebble class places a fraction of the instances.
//!
//! A densely strewn ground is mostly pebbles. Each instance costs the same CPU (collect, the
//! frame's per-mesh bookkeeping, the GPU cull's slot) however small it is, so a class of
//! pebbles at ten times today's density is ten times the instances. A clump draws `members`
//! pebbles per instance: the instance count, the cull and the per-mesh CPU drop by that factor
//! at the same pebble count and the same triangles.
//!
//! - Members sit on a disc around the pivot, base on the ground, each with its own yaw and a
//!   size skewed toward small (`scale = min + (max − min) · uᵇ`, like a class's meshes).
//! - Each member is lifted so that the class's `sink` (a fraction of the *clump's* height)
//!   buries it by the same fraction of *its own* height ([`ClumpSettings::sink`]).
//! - Coarser clump LODs keep only the largest members, scaled up about their own base so the
//!   clump covers the same area (aggregate simplification, Cook, Halstead, Planck & Ryu,
//!   "Stochastic Simplification of Aggregate Detail", SIGGRAPH 2007), and draw them at the
//!   member's coarser LODs.
//!
//! The scatter field needs nothing new: the host's [`ScatterLibrary`](super::field::ScatterLibrary)
//! maps a mesh path to a clump asset (e.g. `scree_00.gltf#clump8`) and the `.scatter.ron` class
//! names that path at its density divided by the member count.

use std::collections::{HashMap, HashSet};

use regia_core::math::{Rng, det_powf, det_sin_cos};
use regia_render::LabVertex;

use super::field::{ScatterMeshAsset, ScatterMeshLod};

/// One grid cell of [`coarse_lod`]: summed position, normal and colour, the vertex count and
/// the first vertex (its UV and tangent are kept).
type CellSum = ([f32; 3], [f32; 3], [f32; 4], f32, LabVertex);

/// Vertex-clustering simplification (Rossignac & Borrel, "Multi-resolution 3D approximations
/// for rendering complex scenes", 1993): every vertex snaps to the cell of a `cells`³ grid over
/// the mesh's bounds, each cell becomes one vertex (mean position, colour and normal, the
/// first vertex's UV and tangent), and triangles that collapse or repeat are dropped.
///
/// Meant for the coarsest LOD of small rocks (a few pixels on screen): 3–4 cells give a
/// 10–40 triangle blob with the rock's silhouette. Returns the input unchanged when it would
/// not get smaller or would vanish.
#[must_use]
pub fn coarse_lod(vertices: &[LabVertex], indices: &[u32], cells: u32) -> (Vec<LabVertex>, Vec<u32>) {
    let cells = cells.max(2);
    let (mut lo, mut hi) = ([f32::INFINITY; 3], [f32::NEG_INFINITY; 3]);
    for v in vertices {
        for k in 0..3 {
            lo[k] = lo[k].min(v.position[k]);
            hi[k] = hi[k].max(v.position[k]);
        }
    }
    let size = [0, 1, 2].map(|k| (hi[k] - lo[k]).max(1e-6));
    let cell_of = |p: [f32; 3]| -> u32 {
        let c = [0, 1, 2].map(|k| (((p[k] - lo[k]) / size[k] * cells as f32) as u32).min(cells - 1));
        (c[2] * cells + c[1]) * cells + c[0]
    };
    let mut slot: HashMap<u32, u32> = HashMap::new();
    let mut sums: Vec<CellSum> = Vec::new();
    let mut remap = Vec::with_capacity(vertices.len());
    for v in vertices {
        let cell = cell_of(v.position);
        let index = *slot.entry(cell).or_insert_with(|| {
            sums.push(([0.0; 3], [0.0; 3], [0.0; 4], 0.0, *v));
            (sums.len() - 1) as u32
        });
        let s = &mut sums[index as usize];
        for k in 0..3 {
            s.0[k] += v.position[k];
            s.1[k] += v.normal[k];
        }
        for k in 0..4 {
            s.2[k] += v.color[k];
        }
        s.3 += 1.0;
        remap.push(index);
    }
    let out_vertices: Vec<LabVertex> = sums
        .iter()
        .map(|(p, n, c, count, first)| {
            let mut v = *first;
            v.position = p.map(|x| x / count);
            let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
            v.normal = if len > 1e-6 {
                n.map(|x| x / len)
            } else {
                first.normal
            };
            v.color = c.map(|x| x / count);
            v
        })
        .collect();
    let mut seen = HashSet::new();
    let mut out_indices = Vec::new();
    for tri in indices.as_chunks::<3>().0 {
        let Some(t) = tri
            .iter()
            .map(|&i| remap.get(i as usize).copied())
            .collect::<Option<Vec<u32>>>()
        else {
            continue;
        };
        if t[0] == t[1] || t[1] == t[2] || t[0] == t[2] {
            continue;
        }
        // Same triangle with the same winding, whatever vertex it starts at.
        let start = (0..3).min_by_key(|&k| t[k]).unwrap_or(0);
        let key = [t[start], t[(start + 1) % 3], t[(start + 2) % 3]];
        if seen.insert(key) {
            out_indices.extend_from_slice(&t);
        }
    }
    if out_indices.is_empty() || out_indices.len() >= indices.len() {
        return (vertices.to_vec(), indices.to_vec());
    }
    (out_vertices, out_indices)
}

/// `asset` with one more, coarser LOD ([`coarse_lod`] of its last LOD with `cells`³ cells),
/// switched to below projected size `screen_size` (the previous last LOD switches there). An
/// asset that already has four LODs, or whose last LOD does not simplify, is returned as is.
#[must_use]
pub fn with_coarse_lod(asset: &ScatterMeshAsset, cells: u32, screen_size: f32) -> ScatterMeshAsset {
    let mut out = asset.clone();
    let Some(last) = asset.lods.last() else {
        return out;
    };
    if asset.lods.len() >= super::field::MAX_SCATTER_LODS {
        return out;
    }
    let (vertices, indices) = coarse_lod(&last.vertices, &last.indices, cells);
    // Nothing coarser (or nothing left: a tiny mesh clusters away) keeps the chain as it is.
    if indices.is_empty() || indices.len() >= last.indices.len() {
        return out;
    }
    if let Some(prev) = out.lods.last_mut() {
        prev.screen_size = screen_size.max(1e-6);
    }
    out.lods.push(ScatterMeshLod {
        vertices,
        indices,
        screen_size: 0.0,
    });
    // Material parts get their own coarse level (or stay empty there), so every part keeps one
    // entry per LOD of the asset.
    for part in &mut out.parts {
        let coarse = match part.lods.last() {
            Some(last) if !last.indices.is_empty() => {
                let (vertices, indices) = coarse_lod(&last.vertices, &last.indices, cells);
                ScatterMeshLod {
                    vertices,
                    indices,
                    screen_size: 0.0,
                }
            }
            _ => ScatterMeshLod {
                vertices: Vec::new(),
                indices: Vec::new(),
                screen_size: 0.0,
            },
        };
        part.lods.push(coarse);
    }
    out
}

/// How a clump is built ([`clump_asset`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClumpSettings {
    /// Pebbles per clump.
    pub members: u32,
    /// Radius of the disc the member centres lie on, in member-mesh units (the clump is then
    /// placed at the class's scale like any mesh).
    pub radius: f32,
    /// Smallest member scale (relative to the source mesh).
    pub min_scale: f32,
    /// Largest member scale.
    pub max_scale: f32,
    /// Member size skew: `scale = min + (max − min) · uᵇ` (larger: more small members).
    pub scale_bias: f32,
    /// The class's `sink`, so each member is buried by that fraction of its own height.
    pub sink: f32,
    /// Placement seed (one clump shape per seed).
    pub seed: u64,
}

impl Default for ClumpSettings {
    fn default() -> Self {
        Self {
            members: 8,
            radius: 2.0,
            min_scale: 0.25,
            max_scale: 1.0,
            scale_bias: 2.0,
            sink: 0.0,
            seed: 1,
        }
    }
}

/// One placed member: where, how large, how turned.
#[derive(Clone, Copy)]
struct Member {
    x: f32,
    z: f32,
    yaw_sin: f32,
    yaw_cos: f32,
    scale: f32,
}

/// Fraction of the members each clump LOD keeps (the largest first), coarsest last.
const KEEP: [f32; super::field::MAX_SCATTER_LODS] = [1.0, 1.0, 0.5, 0.25];

/// The placed members of one clump: where, how large, how turned. The placement draws only
/// from the deterministic kernels ([`det_powf`], [`det_sin_cos`]), never the platform libm, so
/// the same seed gives the same clump bytes on every target (docs/specs/26 § 6).
fn member_placements(settings: &ClumpSettings) -> Vec<Member> {
    let n = settings.members.max(1) as usize;
    let mut rng = Rng::with_stream(settings.seed, 0xc1a3_9000_0000_0001);
    let (lo_s, hi_s) = (
        settings.min_scale.max(1e-3),
        settings.max_scale.max(settings.min_scale.max(1e-3)),
    );
    let mut members: Vec<Member> = (0..n)
        .map(|i| {
            // Stratified angle, square-root radius: an even spread over the disc.
            let a = (i as f32 + rng.f32()) / n as f32 * std::f32::consts::TAU;
            let r = settings.radius * rng.f32().sqrt();
            let yaw = rng.f32() * std::f32::consts::TAU;
            let scale = lo_s + (hi_s - lo_s) * det_powf(rng.f32(), settings.scale_bias.max(0.1));
            let (yaw_sin, yaw_cos) = det_sin_cos(yaw);
            let (a_sin, a_cos) = det_sin_cos(a);
            Member {
                x: r * a_cos,
                z: r * a_sin,
                yaw_sin,
                yaw_cos,
                scale,
            }
        })
        .collect();
    // Largest first: coarser LODs keep a prefix.
    members.sort_by(|a, b| b.scale.total_cmp(&a.scale));
    members
}

/// A clump of `settings.members` copies of `member` (see the module notes). Clump LODs 0–3 draw
/// all, all, half and a quarter of the members, largest first, at member LOD `k` (clamped to
/// the member's last) shifted by `member_lod_offset` (1 skips the member's finest LOD inside a clump, whose
/// pebbles are a fraction of the clump's size); the switch sizes are the member's scaled by
/// the clump-to-member size ratio, so each member changes detail where it would alone.
#[must_use]
pub fn clump_asset(
    member: &ScatterMeshAsset,
    settings: &ClumpSettings,
    member_lod_offset: usize,
) -> ScatterMeshAsset {
    // A hand-built asset may have no LOD chain (`ScatterMeshAsset::validate` rejects it).
    if member.lods.is_empty() {
        return member.clone();
    }
    let n = settings.members.max(1) as usize;
    let hi_s = settings.max_scale.max(settings.min_scale.max(1e-3));
    let members = member_placements(settings);
    let lod0 = &member.lods[0].vertices;
    let (mut min_y, mut max_y) = (f32::INFINITY, f32::NEG_INFINITY);
    let mut radius = 0.0f32;
    for v in lod0 {
        min_y = min_y.min(v.position[1]);
        max_y = max_y.max(v.position[1]);
        radius = radius.max((v.position[0].powi(2) + v.position[2].powi(2)).sqrt());
    }
    let height = (max_y - min_y).max(1e-6);
    let clump_height = height * members.first().map_or(1.0, |m| m.scale);
    let lods = KEEP
        .iter()
        .enumerate()
        .map(|(k, keep)| {
            let count = ((n as f32 * keep).ceil() as usize).clamp(1, n);
            let kept = &members[..count];
            // Same covered area: the kept members grow by sqrt(all area / kept area).
            let area = |ms: &[Member]| ms.iter().map(|m| m.scale * m.scale).sum::<f32>();
            let grow = (area(&members) / area(kept).max(1e-9)).sqrt();
            let src = &member.lods[(k + member_lod_offset).min(member.lods.len() - 1)];
            let mut vertices = Vec::with_capacity(src.vertices.len() * count);
            let mut indices = Vec::with_capacity(src.indices.len() * count);
            for m in kept {
                let s = m.scale * grow;
                // Each member is buried by `sink` of its own height (the field buries the
                // whole clump by `sink` of the tallest member's).
                let lift = settings.sink * (clump_height - height * m.scale).max(0.0) - min_y * s;
                let base = vertices.len() as u32;
                for v in &src.vertices {
                    let mut out = *v;
                    let rot = |p: [f32; 3]| {
                        [
                            p[0] * m.yaw_cos - p[2] * m.yaw_sin,
                            p[1],
                            p[0] * m.yaw_sin + p[2] * m.yaw_cos,
                        ]
                    };
                    let p = rot(v.position);
                    out.position = [p[0] * s + m.x, p[1] * s + lift, p[2] * s + m.z];
                    out.normal = rot(v.normal);
                    let t = rot([v.tangent[0], v.tangent[1], v.tangent[2]]);
                    out.tangent = [t[0], t[1], t[2], v.tangent[3]];
                    vertices.push(out);
                }
                indices.extend(src.indices.iter().map(|i| base + i));
            }
            // Member switch size × (clump radius / member radius).
            let clump_radius = settings.radius + radius * hi_s;
            let ratio = clump_radius / (radius * kept[kept.len() - 1].scale * grow).max(1e-6);
            ScatterMeshLod {
                vertices,
                indices,
                screen_size: src.screen_size * ratio,
            }
        })
        .collect::<Vec<_>>();
    let mut lods = lods;
    // Clump LODs past the member's last switch (member LOD with no switch size) switch at a
    // fraction of the previous clump LOD's size: they only drop members.
    for k in 1..lods.len() {
        if lods[k].screen_size <= 0.0 || lods[k].screen_size >= lods[k - 1].screen_size {
            lods[k].screen_size = lods[k - 1].screen_size * 0.4;
        }
    }
    if let Some(last) = lods.last_mut() {
        last.screen_size = 0.0;
    }
    ScatterMeshAsset {
        lods,
        material: member.material.clone(),
        parts: Vec::new(),
        shadow_lod: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scatter::rock::{RockStyle, procedural_rock};

    /// Fraction of a `n`² grid over `[-half, half]²` (XZ) that the LOD's triangles cover,
    /// seen from above: the clump's footprint on screen when far away.
    fn top_coverage(lod: &ScatterMeshLod, half: f32, n: usize) -> f32 {
        let mut hit = vec![false; n * n];
        let cell = 2.0 * half / n as f32;
        for tri in lod.indices.as_chunks::<3>().0 {
            let p = (*tri).map(|i| {
                let v = lod.vertices[i as usize].position;
                [v[0], v[2]]
            });
            let (lo, hi) = p.iter().fold(([f32::MAX; 2], [f32::MIN; 2]), |(lo, hi), q| {
                (
                    [lo[0].min(q[0]), lo[1].min(q[1])],
                    [hi[0].max(q[0]), hi[1].max(q[1])],
                )
            });
            let to_cell = |x: f32| (((x + half) / cell).floor().max(0.0) as usize).min(n - 1);
            let edge = |a: [f32; 2], b: [f32; 2], c: [f32; 2]| {
                (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0])
            };
            let area = edge(p[0], p[1], p[2]);
            if area.abs() < 1e-12 {
                continue;
            }
            for gz in to_cell(lo[1])..=to_cell(hi[1]) {
                for gx in to_cell(lo[0])..=to_cell(hi[0]) {
                    let c = [-half + (gx as f32 + 0.5) * cell, -half + (gz as f32 + 0.5) * cell];
                    let w = [edge(p[1], p[2], c), edge(p[2], p[0], c), edge(p[0], p[1], c)];
                    if w.iter().all(|&w| w * area >= 0.0) {
                        hit[gz * n + gx] = true;
                    }
                }
            }
        }
        hit.iter().filter(|&&h| h).count() as f32 / (n * n) as f32
    }

    #[test]
    fn clump_lods_keep_the_covered_silhouette() {
        let mut pebble = procedural_rock(3, RockStyle::rounded([0.5, 0.4, 0.3]));
        pebble.lods.truncate(3);
        if let Some(last) = pebble.lods.last_mut() {
            last.screen_size = 0.0;
        }
        let pebble = with_coarse_lod(&pebble, 3, 0.004);
        for seed in 1..6 {
            let clump = clump_asset(
                &pebble,
                &ClumpSettings {
                    members: 8,
                    seed,
                    ..ClumpSettings::default()
                },
                1,
            );
            let half = clump.lods[0]
                .vertices
                .iter()
                .fold(0.0f32, |m, v| m.max(v.position[0].abs()).max(v.position[2].abs()))
                * 1.05;
            let base = top_coverage(&clump.lods[0], half, 200);
            assert!(base > 0.01, "seed {seed}: clump covers {base}");
            for (k, lod) in clump.lods.iter().enumerate().skip(1) {
                let ratio = top_coverage(lod, half, 200) / base;
                // Dropping members without growing the rest covers ~0.4–0.6 at LOD 3.
                assert!(
                    (0.75..=1.4).contains(&ratio),
                    "seed {seed} LOD {k}: covers {ratio} of LOD 0"
                );
            }
        }
    }

    #[test]
    fn coarse_lod_keeps_the_silhouette_with_few_triangles() {
        let rock = procedural_rock(7, RockStyle::angular([0.5, 0.4, 0.3]));
        let lod = &rock.lods[0];
        let (v, i) = coarse_lod(&lod.vertices, &lod.indices, 3);
        assert!(
            i.len() < lod.indices.len() / 4,
            "{} of {}",
            i.len() / 3,
            lod.indices.len() / 3
        );
        assert!(i.len() >= 3 * 6 && i.iter().all(|&k| (k as usize) < v.len()));
        // Bounds within one cell of the source.
        let extent = |vs: &[LabVertex], k: usize| {
            let (a, b) = vs.iter().fold((f32::MAX, f32::MIN), |(a, b), v| {
                (a.min(v.position[k]), b.max(v.position[k]))
            });
            b - a
        };
        for k in 0..3 {
            let (src, out) = (extent(&lod.vertices, k), extent(&v, k));
            assert!(out <= src + 1e-4 && out >= src * 0.3, "axis {k}: {out} vs {src}");
        }
        let asset = with_coarse_lod(&rock, 3, 0.004);
        assert_eq!(asset.lods.len(), (rock.lods.len() + 1).min(4));
        assert!(asset.validate("rock").is_ok());
    }

    #[test]
    fn clumps_merge_members_and_drop_small_ones_far_away() {
        let mut pebble = procedural_rock(3, RockStyle::rounded([0.5, 0.4, 0.3]));
        pebble.lods.truncate(3);
        if let Some(last) = pebble.lods.last_mut() {
            last.screen_size = 0.0;
        }
        let pebble = with_coarse_lod(&pebble, 3, 0.004);
        let settings = ClumpSettings {
            members: 8,
            sink: 0.3,
            ..ClumpSettings::default()
        };
        let clump = clump_asset(&pebble, &settings, 1);
        assert!(clump.validate("clump").is_ok());
        assert_eq!(clump.lods.len(), 4);
        let tris = |l: &ScatterMeshLod| l.indices.len() / 3;
        // LOD 0: all eight members at the member's LOD 1.
        assert_eq!(tris(&clump.lods[0]), 8 * pebble.lods[1].indices.len() / 3);
        // Coarser LODs are cheaper and their switch sizes fall.
        for w in clump.lods.windows(2) {
            assert!(tris(&w[1]) <= tris(&w[0]));
            assert!(w[1].screen_size < w[0].screen_size || w[1].screen_size == 0.0);
        }
        assert!(tris(&clump.lods[3]) * 8 <= tris(&clump.lods[0]));
        // Every member stands on the ground: some vertices below and some above y = 0 after
        // the class buries the clump by `sink` of its height.
        let ys: Vec<f32> = clump.lods[0].vertices.iter().map(|v| v.position[1]).collect();
        let (lo, hi) = ys
            .iter()
            .fold((f32::MAX, f32::MIN), |(a, b), &y| (a.min(y), b.max(y)));
        assert!(lo.abs() < 1e-3 * (hi - lo).max(1.0), "lowest vertex {lo}");
        // Deterministic.
        let again = clump_asset(&pebble, &settings, 1);
        assert_eq!(again.lods[2].vertices.len(), clump.lods[2].vertices.len());
        assert_eq!(
            again.lods[2].vertices[5].position,
            clump.lods[2].vertices[5].position
        );
    }

    /// The `Default` is host-visible: a game building clumps with `..ClumpSettings::default()`
    /// gets the pebble size distribution the default describes. Pins set the bias they pin
    /// explicitly, so this value is never "whatever the last test needed".
    #[test]
    fn clump_settings_default_scale_bias_is_2() {
        assert_eq!(ClumpSettings::default().scale_bias, 2.0);
    }

    #[test]
    fn clump_of_an_asset_without_lods_returns_it_unchanged() {
        let mut rock = procedural_rock(3, RockStyle::rounded([0.5, 0.4, 0.3]));
        rock.lods.clear();
        let out = clump_asset(&rock, &ClumpSettings::default(), 0);
        assert!(out.lods.is_empty());
    }

    /// docs/specs/26 § 6: the member placement (disc position, yaw, scale) draws only from the
    /// deterministic kernels, so these bits are pinned — a platform `powf`/`sin`/`cos` in the
    /// placement changes them and fails here. The values are the det-kernel output; they are
    /// identical on every target (x86_64, aarch64, Windows).
    #[test]
    fn clump_member_placement_bits_are_pinned_for_every_platform() {
        let settings = ClumpSettings {
            members: 8,
            radius: 2.0,
            min_scale: 0.8,
            max_scale: 1.2,
            scale_bias: 2.27,
            sink: 0.0,
            seed: 1,
        };
        let members = member_placements(&settings);
        assert_eq!(members.len(), 8);
        let bits: Vec<u32> = members
            .iter()
            .flat_map(|m| {
                [
                    m.x.to_bits(),
                    m.z.to_bits(),
                    m.yaw_sin.to_bits(),
                    m.yaw_cos.to_bits(),
                    m.scale.to_bits(),
                ]
            })
            .collect();
        // Sorted largest-first by scale; the pin is the exact sequence.
        #[rustfmt::skip]
        let expected: [u32; 40] = [
            3220865497, 3194616873, 3201268932, 1063913839, 1065663667,
            1027410025, 1059696204, 1063763155, 1054445764, 1065018957,
            3192270212, 1044570792, 3212574031, 1043631039, 1064740651,
            3210129837, 3214599229, 3208875964, 3206886329, 1063952783,
            1067734912, 3200379878, 3212359608, 1047691566, 1063916030,
            3190351123, 1054178447, 3210753014, 1056383683, 1063689448,
            1068802430, 1066638209, 3191250818, 1065084251, 1062745113,
            1058821730, 3208793681, 3211252824, 3201903732, 1062119442,
        ];
        assert_eq!(
            bits, expected,
            "placement bits moved: platform libm in the clump placement?"
        );
    }
}
