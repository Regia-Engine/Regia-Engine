// SPDX-License-Identifier: MPL-2.0
//! Frustum culling of the ring grid (ADR-2342, `docs/specs/100-water-extras.md` § 2): the grid is
//! drawn as index ranges ([`super::mesh::RingChunk`]: the inner square, then each ring's four
//! sides), and a range whose displaced world box lies outside the camera frustum is not drawn.
//!
//! The box of a chunk is its offsets from the grid's center, moved to the world by the center,
//! widened horizontally by the swell's largest shift (`Σ H`) and vertically by its largest rise
//! (`Σ A`) around the still level, so a displaced vertex is never outside it. The test is the
//! plane test of Gribb and Hartmann (2001, "Fast Extraction of Viewing Frustum Planes from the
//! World-View-Projection Matrix") against the six planes of the view-projection matrix, with the
//! box's positive vertex (Akenine-Möller, Haines and Hoffman, *Real-Time Rendering*): a box is
//! culled only when it is entirely outside one plane, so culling never hides a visible triangle.

use std::ops::Range;

use regia_core::glam::{Mat4, Vec3, Vec4};

use super::mesh::RingChunk;

/// The six frustum planes `(nx, ny, nz, d)` of a view-projection matrix for a depth range of
/// `0..1` (left, right, bottom, top, near, far); a point is inside when `n · p + d >= 0` for all.
#[must_use]
pub fn frustum_planes(view_proj: &Mat4) -> [Vec4; 6] {
    let (r0, r1, r2, r3) = (
        view_proj.row(0),
        view_proj.row(1),
        view_proj.row(2),
        view_proj.row(3),
    );
    [r3 + r0, r3 - r0, r3 + r1, r3 - r1, r2, r3 - r2]
}

/// `true` when the box `min..max` lies entirely outside one of `planes`.
#[must_use]
pub fn box_outside(planes: &[Vec4; 6], min: Vec3, max: Vec3) -> bool {
    planes.iter().any(|p| {
        let far = Vec3::new(
            if p.x >= 0.0 { max.x } else { min.x },
            if p.y >= 0.0 { max.y } else { min.y },
            if p.z >= 0.0 { max.z } else { min.z },
        );
        p.x * far.x + p.y * far.y + p.z * far.z + p.w < 0.0
    })
}

/// One swell body's copy of the grid: where its indices start and the box it can reach.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CullBody {
    /// First index of this body's copy in the ring index buffer.
    pub index_base: u32,
    /// The still level (world y).
    pub level: f32,
    /// Largest rise or fall from the level (`Σ A`).
    pub rise: f32,
    /// Largest horizontal shift (`Σ H`).
    pub shift: f32,
}

/// The index ranges to draw: every chunk of every body whose world box intersects the frustum,
/// adjacent ranges merged. `center` is the grid's center (x, z). Returns the ranges and the
/// number of chunks culled.
#[must_use]
pub fn visible_ranges(
    chunks: &[RingChunk],
    bodies: &[CullBody],
    center: [f64; 2],
    planes: &[Vec4; 6],
) -> (Vec<Range<u32>>, u32) {
    let mut ranges = Vec::new();
    let culled = visible_ranges_into(chunks, bodies, center, planes, &mut ranges);
    (ranges, culled)
}

/// [`visible_ranges`] writing into `ranges`, which the caller keeps. Clears `ranges` first.
#[must_use]
pub(crate) fn visible_ranges_into(
    chunks: &[RingChunk],
    bodies: &[CullBody],
    center: [f64; 2],
    planes: &[Vec4; 6],
    ranges: &mut Vec<Range<u32>>,
) -> u32 {
    visible_ranges_through_into(chunks, bodies, center, Some(planes), None, ranges).0
}

/// [`visible_ranges_into`] with the frustum test optional (`None`: every chunk passes it) and,
/// for an eye inside a cell, the outdoor-portal test (ADR-8843): a chunk whose box no frustum of
/// `portals` admits (within [`regia_render::viscells::OUTDOOR_NODE_MARGIN_M`]) is left out too.
/// Returns the chunks the frustum left out and those the portals left out.
pub(crate) fn visible_ranges_through_into(
    chunks: &[RingChunk],
    bodies: &[CullBody],
    center: [f64; 2],
    planes: Option<&[Vec4; 6]>,
    portals: Option<&regia_render::viscells::OutdoorPortals>,
    ranges: &mut Vec<Range<u32>>,
) -> (u32, u32) {
    ranges.clear();
    let mut culled = 0u32;
    let mut portal_culled = 0u32;
    for body in bodies {
        for chunk in chunks {
            // The box is formed relative to the center in f32 then moved by the f64 center: the
            // planes are in world space, so the sum is taken in f64 and rounded once.
            let lo = Vec3::new(
                (center[0] + f64::from(chunk.min[0] - body.shift - chunk.snap_pad)) as f32,
                body.level - body.rise,
                (center[1] + f64::from(chunk.min[1] - body.shift - chunk.snap_pad)) as f32,
            );
            let hi = Vec3::new(
                (center[0] + f64::from(chunk.max[0] + body.shift + chunk.snap_pad)) as f32,
                body.level + body.rise,
                (center[1] + f64::from(chunk.max[1] + body.shift + chunk.snap_pad)) as f32,
            );
            if planes.is_some_and(|planes| box_outside(planes, lo, hi)) {
                culled += 1;
                continue;
            }
            if portals.is_some_and(|p| {
                !p.admits_box(
                    lo.to_array(),
                    hi.to_array(),
                    regia_render::viscells::OUTDOOR_NODE_MARGIN_M,
                )
            }) {
                portal_culled += 1;
                continue;
            }
            let start = body.index_base + chunk.first_index;
            let end = start + chunk.index_count;
            match ranges.last_mut() {
                Some(last) if last.end == start => last.end = end,
                _ => ranges.push(start..end),
            }
        }
    }
    (culled, portal_culled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::water_body::mesh::build_ring_grid_chunks;
    use crate::water_body::swell::RingGrid;

    fn grid(m: u32, rings: u32) -> RingGrid {
        RingGrid {
            cell_bits: 0.5f32.to_bits(),
            half_cells: m,
            rings,
        }
    }

    fn view_proj(eye: Vec3, target: Vec3) -> Mat4 {
        let view = regia_render::view_math::look_at_view(eye, target, Vec3::Y);
        let proj = regia_core::glam::camera::rh::proj::directx::perspective(1.0, 16.0 / 9.0, 0.1, 400.0);
        proj * view
    }

    #[test]
    fn chunks_tile_the_index_list_and_bound_their_triangles() {
        for (m, rings) in [(4u32, 4u32), (16, 3), (6, 1), (8, 6)] {
            let g = grid(m, rings);
            let (points, indices, chunks) = build_ring_grid_chunks(&g);
            assert_eq!(chunks.len() as u32, 1 + 4 * (rings - 1));
            let mut cursor = 0u32;
            for c in &chunks {
                assert_eq!(c.first_index, cursor, "chunks follow each other");
                assert!(c.index_count > 0 && c.index_count.is_multiple_of(3));
                cursor += c.index_count;
                for t in indices[c.first_index as usize..(c.first_index + c.index_count) as usize]
                    .as_chunks::<3>()
                    .0
                {
                    for i in t {
                        let p = points[*i as usize];
                        assert!(
                            p[0] >= c.min[0] && p[0] <= c.max[0] && p[1] >= c.min[1] && p[1] <= c.max[1],
                            "vertex {p:?} outside {c:?} (m {m}, rings {rings})"
                        );
                    }
                }
            }
            assert_eq!(cursor as usize, indices.len());
        }
    }

    #[test]
    fn plane_test_matches_a_known_view() {
        // Looking down -z from the origin: the box ahead is in, behind and far left are out.
        let planes = frustum_planes(&view_proj(Vec3::ZERO, Vec3::new(0.0, 0.0, -1.0)));
        let b =
            |x: f32, y: f32, z: f32, h: f32| (Vec3::new(x - h, y - h, z - h), Vec3::new(x + h, y + h, z + h));
        let (lo, hi) = b(0.0, 0.0, -10.0, 1.0);
        assert!(!box_outside(&planes, lo, hi));
        let (lo, hi) = b(0.0, 0.0, 10.0, 1.0);
        assert!(box_outside(&planes, lo, hi), "behind the eye");
        let (lo, hi) = b(-200.0, 0.0, -10.0, 1.0);
        assert!(box_outside(&planes, lo, hi), "far left");
        let (lo, hi) = b(0.0, 0.0, -500.0, 1.0);
        assert!(box_outside(&planes, lo, hi), "beyond the far plane");
        // A box straddling the left plane stays.
        let (lo, hi) = b(-6.0, 0.0, -10.0, 4.0);
        assert!(!box_outside(&planes, lo, hi));
    }

    /// Culling is conservative: every triangle with a vertex inside the frustum is in a drawn
    /// range, over a sweep of headings and a displaced box; looking away culls most of the grid.
    #[test]
    fn culling_keeps_every_visible_triangle() {
        let g = grid(8, 5);
        let (points, indices, chunks) = build_ring_grid_chunks(&g);
        let body = CullBody {
            index_base: 0,
            level: 2.0,
            rise: 1.5,
            shift: 0.8,
        };
        let center = [100.0f64, -40.0];
        let eye = Vec3::new(100.0, 12.0, -40.0);
        let mut culled_looking_away = 0;
        for step in 0..24 {
            let a = step as f32 * std::f32::consts::TAU / 24.0;
            let target = eye + Vec3::new(a.cos(), -0.3, a.sin()) * 10.0;
            let vp = view_proj(eye, target);
            let planes = frustum_planes(&vp);
            let (ranges, culled) = visible_ranges(&chunks, &[body], center, &planes);
            if step == 0 {
                culled_looking_away = culled;
            }
            let drawn = |tri: usize| {
                ranges
                    .iter()
                    .any(|r| (r.start as usize..r.end as usize).contains(&(tri * 3)))
            };
            for (tri, t) in indices.as_chunks::<3>().0.iter().enumerate() {
                // Sample the triangle's vertices at the displacement extremes.
                let visible = t.iter().any(|i| {
                    let p = points[*i as usize];
                    [-1.0f32, 1.0].iter().any(|dy| {
                        [-1.0f32, 1.0].iter().any(|dx| {
                            let w = Vec4::new(
                                (center[0] + f64::from(p[0] + dx * body.shift)) as f32,
                                body.level + dy * body.rise,
                                (center[1] + f64::from(p[1])) as f32,
                                1.0,
                            );
                            let c = vp * w;
                            c.w > 0.0 && c.x.abs() <= c.w && c.y.abs() <= c.w && c.z >= 0.0 && c.z <= c.w
                        })
                    })
                });
                assert!(
                    !visible || drawn(tri),
                    "step {step}: visible triangle {tri} culled"
                );
            }
        }
        assert!(
            culled_looking_away > 2,
            "the camera at step 0 sees only part of the grid"
        );
    }

    #[test]
    fn ranges_merge_across_bodies_and_skip_culled_chunks() {
        let g = grid(4, 3);
        let (_, indices, chunks) = build_ring_grid_chunks(&g);
        let n = indices.len() as u32;
        let bodies = [
            CullBody {
                index_base: 0,
                level: 0.0,
                rise: 0.0,
                shift: 0.0,
            },
            CullBody {
                index_base: n,
                level: 0.0,
                rise: 0.0,
                shift: 0.0,
            },
        ];
        // Everything inside: one range per body merge into one run.
        let planes = frustum_planes(&view_proj(Vec3::new(0.0, 500.0, 5.0), Vec3::ZERO)); // far plane at 400 → all out
        let (ranges, culled) = visible_ranges(&chunks, &bodies, [0.0, 0.0], &planes);
        assert!(ranges.is_empty());
        assert_eq!(culled as usize, 2 * chunks.len());
        let near = frustum_planes(&view_proj(Vec3::new(0.0, 30.0, 5.0), Vec3::ZERO));
        let (ranges, culled) = visible_ranges(&chunks, &bodies, [0.0, 0.0], &near);
        assert_eq!(culled, 0, "a downward view of a 12 m grid sees all of it");
        assert_eq!(ranges, vec![0..2 * n]);
    }

    /// Spec 100 § 6: a chunk's box is widened by its ring's snap pad `2^(r+1) c` (zero for the
    /// inner square and for a grid that does not snap), and the pad holds every vertex of the
    /// chunk after the per-ring snap moves it, for eyes near and far from the world origin.
    #[test]
    fn snap_pad_holds_every_snapped_vertex_of_its_chunk() {
        let cell = 0.5f32;
        let still = grid(6, 5);
        assert!(!still.snaps_rings());
        let (_, _, chunks) = build_ring_grid_chunks(&still);
        assert!(chunks.iter().all(|c| c.snap_pad == 0.0));
        let g = grid(16, 5);
        assert!(g.snaps_rings());
        let (points, indices, chunks) = build_ring_grid_chunks(&g);
        assert_eq!(chunks[0].snap_pad, 0.0, "the inner square does not move");
        for (i, chunk) in chunks.iter().enumerate().skip(1) {
            let ring = 1 + (i - 1) / 4;
            assert_eq!(chunk.snap_pad, cell * (1u32 << (ring + 1)) as f32, "chunk {i}");
        }
        let snap = 2.0 * f64::from(cell);
        for eye in [[0.3, -0.2], [7.9, 13.1], [-41.7, 2.6], [123_456.7, -98_765.4]] {
            let origin = [(eye[0] / snap).floor() * snap, (eye[1] / snap).floor() * snap];
            let shifts = g.ring_shifts(origin, eye);
            for (i, chunk) in chunks.iter().enumerate() {
                let range = chunk.first_index as usize..(chunk.first_index + chunk.index_count) as usize;
                for &v in &indices[range] {
                    let local = points[v as usize];
                    let d = g.ring_shift(&shifts, local);
                    for axis in 0..2 {
                        let moved = local[axis] + d[axis];
                        assert!(
                            moved >= chunk.min[axis] - chunk.snap_pad
                                && moved <= chunk.max[axis] + chunk.snap_pad,
                            "chunk {i} vertex {local:?} moved to {moved} outside the padded box at {eye:?}"
                        );
                    }
                }
            }
        }
    }
}
