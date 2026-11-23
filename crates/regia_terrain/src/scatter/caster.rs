// SPDX-License-Identifier: MPL-2.0
//! Lower-detail shadow casters for scatter classes (ADR-3146, `book/agents/scatter-forest.md`).
//!
//! [`ScatterCasterHint`] extends the field-wide shadow LOD hint
//! ([`super::field::ScatterField::set_shadow_lod_hint`]): per class, from a drawn LOD on, so that near
//! shadows keep their detail. A far instance is a far caster, so choosing by the drawn LOD thins the far
//! cascades without the frame's cascade jobs knowing about it: the shadow moves to a shadow-only draw, as
//! it does for an asset's own shadow LOD.
//!
//! Where a chain has no coarser LOD to cast from, [`reduce_cards`] builds one: leaf cards are grouped by
//! place in the crown, each group keeps one card, and that card grows to the group's area. This is the
//! area-preserving element removal of Cook, Halstead, Planck & Ryu, "Stochastic Simplification of
//! Aggregate Detail" (SIGGRAPH 2007). For independently placed cards the fraction a crown covers is
//! `1 - exp(-n a / A)`, which depends only on the total area `n a`, so the coverage holds while the
//! triangle count falls; [`caster_coverage`] measures it.

use regia_core::glam::Vec3;
use regia_render::{ALPHA_MASK, LabMaterial, LabVertex, ShadowLodHint};
use serde::Serialize;

/// A class's shadow casters past a drawn LOD. `Default` changes nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct ScatterCasterHint {
    /// The hint applies to instances drawn at this LOD or coarser; finer ones cast as without it.
    pub from_lod: u8,
    /// Coarsest LOD those instances may cast from ([`ShadowLodHint::max_lod`]; [`u8::MAX`]: no cap).
    pub max_lod: u8,
    /// Finest LOD those instances may cast from ([`ShadowLodHint::min_lod`]; 0: no floor).
    pub min_lod: u8,
    /// Of the leaf cards those instances cast, keep one in this many ([`reduce_cards`]); 0 and 1 keep
    /// them all. A part keeps at least `MIN_KEPT_CARDS` (64) cards, so a sparse part thins less and one
    /// with fewer than 128 cards (a far billboard, a sapling) casts whole.
    pub leaf_keep_one_in: u8,
}

impl Default for ScatterCasterHint {
    fn default() -> Self {
        Self {
            from_lod: 0,
            max_lod: u8::MAX,
            min_lod: 0,
            leaf_keep_one_in: 1,
        }
    }
}

impl ScatterCasterHint {
    /// Most cards a [`Self::leaf_keep_one_in`] may remove from each one kept.
    pub const MAX_KEEP_ONE_IN: u8 = 16;

    /// Whether this hint changes no caster.
    #[must_use]
    pub fn is_neutral(&self) -> bool {
        self.max_lod == u8::MAX && self.min_lod == 0 && self.keep_one_in() <= 1
    }

    /// The clamp on the shadow LOD ([`Self::max_lod`], [`Self::min_lod`]).
    #[must_use]
    pub fn lod(&self) -> ShadowLodHint {
        ShadowLodHint {
            max_lod: self.max_lod,
            min_lod: self.min_lod,
        }
    }

    /// [`Self::leaf_keep_one_in`], at least 1 and at most [`Self::MAX_KEEP_ONE_IN`].
    #[must_use]
    pub fn keep_one_in(&self) -> u8 {
        self.leaf_keep_one_in.clamp(1, Self::MAX_KEEP_ONE_IN)
    }
}

/// Fewest cards a thinned part keeps. Few kept cards leave the shadow to chance: one card's orientation
/// decides much of its group's projected area under a given sun, and a crown of 150 cards thinned to 38
/// moved its coverage by 18 % under a low sun (ADR-3146). Below twice this a part casts whole: it is a
/// billboard, a few crossed planes or a sparse sapling, whose silhouette is its cards.
const MIN_KEPT_CARDS: usize = 64;

/// One connected piece of a leaf mesh: its triangles (index of the first index, in order), area and
/// area-weighted centre.
struct Card {
    triangles: Vec<usize>,
    area: f32,
    centre: Vec3,
    /// Sum of the triangles' area-weighted normals (twice the area each, unnormalised).
    normal: Vec3,
    morton: u64,
}

/// `1 / sqrt(2)`.
const S2: f32 = std::f32::consts::FRAC_1_SQRT_2;
/// `1 / sqrt(3)`.
const S3: f32 = 0.577_350_3;
/// Directions the kept cards' projected area tracks (sign does not matter: a card casts from both
/// sides): the three axes, the six face diagonals and the four body diagonals of a cube.
const TRACKED: [[f32; 3]; 13] = [
    [1.0, 0.0, 0.0],
    [0.0, 1.0, 0.0],
    [0.0, 0.0, 1.0],
    [S2, S2, 0.0],
    [S2, -S2, 0.0],
    [S2, 0.0, S2],
    [S2, 0.0, -S2],
    [0.0, S2, S2],
    [0.0, S2, -S2],
    [S3, S3, S3],
    [S3, S3, -S3],
    [S3, -S3, S3],
    [-S3, S3, S3],
];

/// A card's projected area along each [`TRACKED`] direction, were its area `area`.
fn projected(card: &Card, area: f32) -> [f32; 13] {
    let n = card.normal.normalize_or(Vec3::Y);
    TRACKED.map(|d| area * n.dot(Vec3::from_array(d)).abs())
}

/// Spreads the low 21 bits of `v` to every third bit.
fn spread3(v: u64) -> u64 {
    let mut x = v & 0x1f_ffff;
    x = (x | x << 32) & 0x001f_0000_0000_ffff;
    x = (x | x << 16) & 0x001f_0000_ff00_00ff;
    x = (x | x << 8) & 0x100f_00f0_0f00_f00f;
    x = (x | x << 4) & 0x10c3_0c30_c30c_30c3;
    (x | x << 2) & 0x1249_2492_4924_9249
}

/// Keeps one card in `keep_one_in` of a leaf mesh, each kept card grown about its centre to the area of
/// its group; vertices no triangle references (the field's bounds corners) stay. `None` when the mesh
/// has too few separate cards to thin; a sparse one keeps at least 64 cards whatever `keep_one_in` asks. Deterministic: the same mesh gives the same bytes.
#[must_use]
pub fn reduce_cards(
    vertices: &[LabVertex],
    indices: &[u32],
    keep_one_in: u8,
) -> Option<(Vec<LabVertex>, Vec<u32>)> {
    let keep = usize::from(keep_one_in.clamp(1, ScatterCasterHint::MAX_KEEP_ONE_IN));
    if keep <= 1 {
        return None;
    }
    let (triangles, _) = indices.as_chunks::<3>();
    if triangles.iter().flatten().any(|&i| i as usize >= vertices.len()) {
        return None;
    }
    // Cards: the connected components of the triangles over shared vertices.
    let mut parent: Vec<u32> = (0..vertices.len() as u32).collect();
    fn root(parent: &mut [u32], mut i: u32) -> u32 {
        while parent[i as usize] != i {
            let grand = parent[parent[i as usize] as usize];
            parent[i as usize] = grand;
            i = grand;
        }
        i
    }
    for tri in triangles {
        let a = root(&mut parent, tri[0]);
        for &other in &tri[1..] {
            let b = root(&mut parent, other);
            parent[b as usize] = a;
        }
    }
    let position = |i: u32| Vec3::from_array(vertices[i as usize].position);
    let mut slot_of: std::collections::HashMap<u32, usize> = std::collections::HashMap::new();
    let mut cards: Vec<Card> = Vec::new();
    for (t, tri) in triangles.iter().enumerate() {
        let slot = *slot_of.entry(root(&mut parent, tri[0])).or_insert_with(|| {
            cards.push(Card {
                triangles: Vec::new(),
                area: 0.0,
                centre: Vec3::ZERO,
                normal: Vec3::ZERO,
                morton: 0,
            });
            cards.len() - 1
        });
        let (a, b, c) = (position(tri[0]), position(tri[1]), position(tri[2]));
        let cross = (b - a).cross(c - a);
        let area = 0.5 * cross.length();
        let card = &mut cards[slot];
        card.triangles.push(t);
        card.area += area;
        card.normal += if card.normal.dot(cross) < 0.0 {
            -cross
        } else {
            cross
        };
        card.centre += (a + b + c) / 3.0 * area;
    }
    let keep = keep.min(cards.len() / MIN_KEPT_CARDS);
    if keep <= 1 {
        return None;
    }
    // Order the cards along a Morton curve through the crown, so each group is one place in it and the
    // kept cards stay spread as the whole set was.
    let (mut lo, mut hi) = (Vec3::splat(f32::INFINITY), Vec3::splat(f32::NEG_INFINITY));
    for card in &mut cards {
        card.centre /= card.area.max(1e-9);
        lo = lo.min(card.centre);
        hi = hi.max(card.centre);
    }
    let extent = (hi - lo).max(Vec3::splat(1e-6));
    // The crown's area-weighted centre, and how far each card reaches from it: a grown card is pulled
    // towards the centre, or the crown's silhouette swells by the growth and the shadow reads denser
    // (ADR-3146).
    let total_area: f32 = cards.iter().map(|c| c.area).sum();
    let crown_centre = cards.iter().map(|c| c.centre * c.area).sum::<Vec3>() / total_area.max(1e-9);
    let radius = |card: &Card| {
        card.triangles
            .iter()
            .flat_map(|&t| triangles[t])
            .map(|i| position(i).distance(card.centre))
            .fold(0.0f32, f32::max)
    };
    let reach = |card: &Card| card.centre.distance(crown_centre) + radius(card);
    for card in &mut cards {
        let unit = (card.centre - lo) / extent;
        let quantize = |v: f32| (v.clamp(0.0, 1.0) * 2_097_151.0) as u64;
        card.morton =
            spread3(quantize(unit.x)) | spread3(quantize(unit.y)) << 1 | spread3(quantize(unit.z)) << 2;
    }
    let mut order: Vec<usize> = (0..cards.len()).collect();
    order.sort_by_key(|&c| (cards[c].morton, c));
    let mut out_vertices: Vec<LabVertex> = Vec::new();
    let mut out_indices: Vec<u32> = Vec::new();
    // Error diffusion over orientation: in each group keep the card whose grown projected area, along
    // every tracked direction, brings the kept running total closest to the whole mesh's. A sparse crown
    // keeps few cards, and one card's orientation would otherwise decide how much of its group's shadow
    // survives under a given sun.
    let mut carried = [0.0f32; 13];
    for group in order.chunks(keep) {
        let group_area: f32 = group.iter().map(|&c| cards[c].area).sum();
        let mut target = carried;
        for &c in group {
            for (t, p) in target.iter_mut().zip(projected(&cards[c], cards[c].area)) {
                *t += p;
            }
        }
        let miss = |c: usize| {
            projected(&cards[c], group_area)
                .iter()
                .zip(&target)
                .map(|(p, t)| (t - p) * (t - p))
                .sum::<f32>()
        };
        // The smallest miss; the larger card on a tie (its texture detail survives the growth best),
        // then the first.
        let kept = group.iter().copied().fold(group[0], |best, c| {
            let (m, b) = (miss(c), miss(best));
            if m < b || (m == b && cards[c].area > cards[best].area) {
                c
            } else {
                best
            }
        });
        let kept_projected = projected(&cards[kept], group_area);
        carried = std::array::from_fn(|k| target[k] - kept_projected[k]);
        let card = &cards[kept];
        let scale = (group_area / card.area.max(1e-9)).sqrt();
        let group_reach = group.iter().map(|&c| reach(&cards[c])).fold(0.0f32, f32::max);
        let from_centre = card.centre - crown_centre;
        // Halfway between the card's own place and the place where its grown edge meets the group's
        // reach: a card's 3D radius overstates how far it reaches in a projection, so the full pull thins
        // the outline (measured: no pull +8 % unit / +12 % field coverage, full pull -6 % / -9 %).
        let distance = from_centre.length();
        let edge_at_reach = (group_reach - radius(card) * scale).clamp(0.0, distance);
        let grown_centre = crown_centre + from_centre.normalize_or_zero() * 0.5 * (distance + edge_at_reach);
        let mut remap: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
        for &t in &card.triangles {
            for &index in &triangles[t] {
                let new = *remap.entry(index).or_insert_with(|| {
                    let mut v = vertices[index as usize];
                    let p = Vec3::from_array(v.position);
                    v.position = (grown_centre + (p - card.centre) * scale).to_array();
                    out_vertices.push(v);
                    (out_vertices.len() - 1) as u32
                });
                out_indices.push(new);
            }
        }
    }
    let mut referenced = vec![false; vertices.len()];
    for &index in indices {
        referenced[index as usize] = true;
    }
    out_vertices.extend(
        vertices
            .iter()
            .zip(&referenced)
            .filter(|(_, used)| !**used)
            .map(|(v, _)| *v),
    );
    Some((out_vertices, out_indices))
}

/// One mesh a caster holds, for [`caster_coverage`].
#[derive(Clone, Copy)]
pub struct CasterPart<'a> {
    /// The mesh's vertices.
    pub vertices: &'a [LabVertex],
    /// The mesh's triangle list.
    pub indices: &'a [u32],
    /// Its material: an alpha-masked one is tested against its albedo alpha, as alpha-tested shadows
    /// do; `None` casts the whole triangles.
    pub material: Option<&'a LabMaterial>,
}

/// Fraction of a square, side `2 * half_extent` about `centre` and facing the sun, that a caster's parts
/// cover: what its shadow covers in a shadow map whose texels are `2 * half_extent / cells` wide, with
/// each texel taken as covered or not at its centre. The far-cascade check of [`reduce_cards`].
#[must_use]
pub fn caster_coverage(
    parts: &[CasterPart<'_>],
    to_sun: [f32; 3],
    centre: [f32; 3],
    half_extent: f32,
    cells: usize,
) -> f32 {
    let f = Vec3::from_array(to_sun).normalize_or(Vec3::Y);
    let helper = if f.y.abs() > 0.9 { Vec3::X } else { Vec3::Y };
    let r = helper.cross(f).normalize_or(Vec3::X);
    let u = f.cross(r);
    let centre = Vec3::from_array(centre);
    let half = half_extent.max(1e-6);
    let size = cells.max(1);
    let scale = size as f32 / (2.0 * half);
    let mut covered = vec![false; size * size];
    for part in parts {
        let project = |i: u32| {
            let l = Vec3::from_array(part.vertices[i as usize].position) - centre;
            [(l.dot(r) + half) * scale, (l.dot(u) + half) * scale]
        };
        let mask = part
            .material
            .filter(|m| m.alpha_mode == ALPHA_MASK)
            .and_then(|m| m.albedo.as_ref().map(|t| (t, m.alpha_cutoff)));
        for tri in part.indices.as_chunks::<3>().0 {
            if tri.iter().any(|&i| i as usize >= part.vertices.len()) {
                continue;
            }
            let (a, b, c) = (project(tri[0]), project(tri[1]), project(tri[2]));
            let area = (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0]);
            if area.abs() < 1e-9 {
                continue;
            }
            let range = |v: [f32; 3]| {
                let lo = v[0].min(v[1]).min(v[2]).floor().max(0.0) as usize;
                let hi = (v[0].max(v[1]).max(v[2]).ceil().max(0.0) as usize).min(size);
                lo..hi
            };
            let uv = [tri[0], tri[1], tri[2]].map(|i| part.vertices[i as usize].uv);
            for y in range([a[1], b[1], c[1]]) {
                for x in range([a[0], b[0], c[0]]) {
                    if covered[y * size + x] {
                        continue;
                    }
                    let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
                    let w0 = ((b[0] - px) * (c[1] - py) - (b[1] - py) * (c[0] - px)) / area;
                    let w1 = ((c[0] - px) * (a[1] - py) - (c[1] - py) * (a[0] - px)) / area;
                    let w2 = 1.0 - w0 - w1;
                    if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
                        continue;
                    }
                    if let Some((texture, cutoff)) = mask {
                        let s = w0 * uv[0][0] + w1 * uv[1][0] + w2 * uv[2][0];
                        let t = w0 * uv[0][1] + w1 * uv[1][1] + w2 * uv[2][1];
                        let (width, height) = (texture.width.max(1) as usize, texture.height.max(1) as usize);
                        let tx = (s.rem_euclid(1.0) * width as f32) as usize % width;
                        let ty = (t.rem_euclid(1.0) * height as f32) as usize % height;
                        let alpha = texture
                            .rgba
                            .get((ty * width + tx) * 4 + 3)
                            .map_or(1.0, |&a| f32::from(a) / 255.0);
                        if alpha < cutoff {
                            continue;
                        }
                    }
                    covered[y * size + x] = true;
                }
            }
        }
    }
    covered.iter().filter(|c| **c).count() as f32 / (size * size) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `count` separate square cards of side `side` with a seeded scatter over a crown of radius `r`.
    fn crown(count: usize, side: f32, r: f32) -> (Vec<LabVertex>, Vec<u32>) {
        let mut vertices = Vec::new();
        let mut indices = Vec::new();
        let mut state = 0x5eed_u64;
        let mut next = || {
            state = state.wrapping_mul(0x9e37_79b9_7f4a_7c15).rotate_left(23) ^ 0x1234_5678;
            ((state >> 40) as f32 / 16_777_216.0) * 2.0 - 1.0
        };
        for _ in 0..count {
            let centre = Vec3::new(next(), next(), next()).normalize_or(Vec3::Y) * r * next().abs().cbrt();
            let (a, b) = (
                Vec3::new(next(), next(), next()).normalize_or(Vec3::X),
                Vec3::new(next(), next(), next()).normalize_or(Vec3::Z),
            );
            let (a, b) = (a * side * 0.5, a.cross(b).normalize_or(Vec3::Y) * side * 0.5);
            let base = vertices.len() as u32;
            for (corner, uv) in [
                (centre - a - b, [0.0, 1.0]),
                (centre + a - b, [1.0, 1.0]),
                (centre + a + b, [1.0, 0.0]),
                (centre - a + b, [0.0, 0.0]),
            ] {
                vertices.push(LabVertex::from_pos_normal_uv(
                    corner.to_array(),
                    [0.0, 1.0, 0.0],
                    uv,
                ));
            }
            indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
        }
        (vertices, indices)
    }

    fn area(vertices: &[LabVertex], indices: &[u32]) -> f32 {
        indices
            .as_chunks::<3>()
            .0
            .iter()
            .map(|t| {
                let p = |i: u32| Vec3::from_array(vertices[i as usize].position);
                0.5 * (p(t[1]) - p(t[0])).cross(p(t[2]) - p(t[0])).length()
            })
            .sum()
    }

    #[test]
    fn reduction_keeps_one_card_in_n_and_the_total_area() {
        let (vertices, indices) = crown(400, 0.5, 4.0);
        let (kept_v, kept_i) = reduce_cards(&vertices, &indices, 4).expect("400 cards thin");
        assert_eq!(kept_i.len(), indices.len() / 4);
        let (before, after) = (area(&vertices, &indices), area(&kept_v, &kept_i));
        assert!(
            (after - before).abs() / before < 1e-3,
            "area {before} became {after}"
        );
    }

    #[test]
    fn reduction_is_deterministic_and_keeps_unreferenced_vertices() {
        let (mut vertices, indices) = crown(400, 0.5, 4.0);
        let corner = LabVertex::from_pos_normal_uv([9.0, 9.0, 9.0], [0.0, 1.0, 0.0], [0.0, 0.0]);
        vertices.push(corner);
        let a = reduce_cards(&vertices, &indices, 3).expect("thins");
        let b = reduce_cards(&vertices, &indices, 3).expect("thins");
        let bits = |v: &[LabVertex]| v.iter().map(|x| x.position.map(f32::to_bits)).collect::<Vec<_>>();
        assert_eq!(bits(&a.0), bits(&b.0));
        assert_eq!(a.1, b.1);
        assert_eq!(a.0.last().map(|v| v.position), Some([9.0, 9.0, 9.0]));
    }

    #[test]
    fn a_billboard_or_a_welded_mesh_does_not_thin() {
        let (vertices, indices) = crown(3, 0.5, 4.0);
        assert!(
            reduce_cards(&vertices, &indices, 4).is_none(),
            "three cards cast whole"
        );
        assert!(
            reduce_cards(&vertices, &indices, 1).is_none(),
            "one in one is no change"
        );
        // A sparse crown keeps 64 cards or more: 150 cards asked one in four keep one in two.
        let (sparse_v, sparse_i) = crown(150, 0.5, 4.0);
        let (_, kept) = reduce_cards(&sparse_v, &sparse_i, 4).expect("150 cards thin");
        assert_eq!(kept.len(), 75 * 6);
        let (few_v, few_i) = crown(127, 0.5, 4.0);
        assert!(
            reduce_cards(&few_v, &few_i, 4).is_none(),
            "under 128 cards cast whole"
        );
        // One strip of many triangles is one card.
        let strip: Vec<LabVertex> = (0..64)
            .map(|i| {
                LabVertex::from_pos_normal_uv([i as f32, (i % 2) as f32, 0.0], [0.0, 0.0, 1.0], [0.0, 0.0])
            })
            .collect();
        let tris: Vec<u32> = (0..62u32).flat_map(|i| [i, i + 1, i + 2]).collect();
        assert!(reduce_cards(&strip, &tris, 4).is_none());
    }

    #[test]
    fn an_index_past_the_vertices_does_not_thin_or_panic() {
        let (vertices, mut indices) = crown(100, 0.5, 4.0);
        indices[7] = vertices.len() as u32 + 3;
        assert!(reduce_cards(&vertices, &indices, 4).is_none());
        let parts = [CasterPart {
            vertices: &vertices,
            indices: &indices,
            material: None,
        }];
        let coverage = caster_coverage(&parts, [0.0, 1.0, 0.0], [0.0; 3], 5.0, 32);
        assert!(coverage > 0.0, "the good triangles still count");
    }

    /// The bound the far cascades rely on (ADR-3146): over crown densities from sparse to dense and three
    /// sun elevations, one card in four (one in two for the sparse crown, held at 64 kept cards) keeps the
    /// coverage within 6 % of the whole mesh's (worst measured 3.3 %). Orientation is tracked, so no sun
    /// direction loses its shadow, and the outline is held, so the crown does not swell.
    #[test]
    fn a_thinned_crown_keeps_its_coverage() {
        let mut worst = (0.0f32, String::new());
        for (count, side) in [(150, 0.6), (400, 0.6), (900, 0.5), (2500, 0.4)] {
            let (vertices, indices) = crown(count, side, 4.0);
            let (kept_v, kept_i) = reduce_cards(&vertices, &indices, 4).expect("thins");
            for to_sun in [[0.0, 1.0, 0.0], [0.5, 0.7, 0.2], [0.9, 0.35, -0.2]] {
                let cover = |v: &[LabVertex], i: &[u32]| {
                    let parts = [CasterPart {
                        vertices: v,
                        indices: i,
                        material: None,
                    }];
                    caster_coverage(&parts, to_sun, [0.0; 3], 5.0, 96)
                };
                let (whole, thin) = (cover(&vertices, &indices), cover(&kept_v, &kept_i));
                let off = (thin - whole).abs() / whole.max(1e-6);
                eprintln!(
                    "{count} cards, sun {to_sun:?}: {whole:.4} -> {thin:.4} ({:.1} %)",
                    off * 100.0
                );
                if off > worst.0 {
                    worst = (
                        off,
                        format!("{count} cards, sun {to_sun:?}: {whole} became {thin}"),
                    );
                }
            }
        }
        assert!(worst.0 < 0.06, "{}", worst.1);
    }

    #[test]
    fn a_neutral_hint_changes_nothing_and_the_factor_clamps() {
        assert!(ScatterCasterHint::default().is_neutral());
        let hint = ScatterCasterHint {
            leaf_keep_one_in: 200,
            ..ScatterCasterHint::default()
        };
        assert_eq!(hint.keep_one_in(), ScatterCasterHint::MAX_KEEP_ONE_IN);
        assert!(!hint.is_neutral());
        assert_eq!(ScatterCasterHint::default().lod(), ShadowLodHint::default());
    }

    #[test]
    fn coverage_reads_the_alpha_mask() {
        let (vertices, indices) = crown(1, 8.0, 0.0);
        let texture = regia_render::LabTexture {
            width: 2,
            height: 1,
            rgba: vec![255, 255, 255, 255, 255, 255, 255, 0],
        };
        let masked = LabMaterial {
            alpha_mode: ALPHA_MASK,
            alpha_cutoff: 0.5,
            albedo: Some(texture),
            ..LabMaterial::default()
        };
        let cover = |material: Option<&LabMaterial>| {
            let parts = [CasterPart {
                vertices: &vertices,
                indices: &indices,
                material,
            }];
            let normal = {
                let p = |i: usize| Vec3::from_array(vertices[i].position);
                (p(1) - p(0)).cross(p(3) - p(0)).normalize()
            };
            caster_coverage(&parts, normal.to_array(), [0.0; 3], 5.0, 64)
        };
        let whole = cover(None);
        let half = cover(Some(&masked));
        assert!((half / whole - 0.5).abs() < 0.05, "{half} of {whole}");
    }
}
