// SPDX-License-Identifier: MPL-2.0
//! Import-time mesh cleaning: coincident duplicate triangles.
//!
//! Some exported meshes carry the same triangle twice: the same three corner positions with
//! the same winding, often with a different UV layout on the second copy (DO27's X-11/X-12/
//! X-31 cargo box has every panel twice, the copy's UVs transposed and stretched 3.6:1). Two
//! coplanar triangles fight in the depth test: the raster shows whichever copy wins per pixel,
//! in bands that shift with the interpolation, while a path tracer shows the first one its
//! acceleration structure hits. On the diamond-plate box that read as a herringbone moiré.
//!
//! [`drop_coincident_triangles`] keeps the first copy in index order and drops the rest.
//! Positions compare bit for bit, so only exact duplicates go: two surfaces a hair apart
//! (decals, panel insets) stay. The opposite winding is a different triangle (a back face
//! authored on purpose on a single-sided material) and is kept.

use std::collections::HashSet;

/// A triangle's corners as position bits, rotated so the smallest corner comes first (the
/// winding, and so the facing, is kept: `abc`, `bca` and `cab` share a key, `acb` does not).
fn triangle_key(positions: &[[f32; 3]], tri: [u32; 3]) -> Option<[[u32; 3]; 3]> {
    let corner = |i: u32| -> Option<[u32; 3]> {
        let p = positions.get(i as usize)?;
        // +0.0 and −0.0 are the same point.
        Some(p.map(|c| if c == 0.0 { 0 } else { c.to_bits() }))
    };
    let c = [corner(tri[0])?, corner(tri[1])?, corner(tri[2])?];
    let first = (0..3).min_by_key(|&i| c[i]).unwrap_or(0);
    Some([c[first], c[(first + 1) % 3], c[(first + 2) % 3]])
}

/// Removes every triangle of `indices` (a triangle list) whose three corner positions, with
/// the same winding, repeat an earlier triangle's, and returns how many were removed. A
/// trailing partial triangle and triangles with an out-of-range index are kept as they are.
///
/// Vertices are not touched: a dropped copy's vertices stay in the buffer, unreferenced.
pub fn drop_coincident_triangles(positions: &[[f32; 3]], indices: &mut Vec<u32>) -> usize {
    let (tris, rest) = indices.as_chunks::<3>();
    let mut seen: HashSet<[[u32; 3]; 3]> = HashSet::with_capacity(tris.len());
    let mut kept = Vec::with_capacity(indices.len());
    let mut removed = 0;
    for &tri in tris {
        if let Some(key) = triangle_key(positions, tri)
            && !seen.insert(key)
        {
            removed += 1;
            continue;
        }
        kept.extend_from_slice(&tri);
    }
    if removed == 0 {
        return 0;
    }
    kept.extend_from_slice(rest);
    *indices = kept;
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unit quad at x = 0.6 as two triangles, the box panel's layout.
    fn quad() -> Vec<[f32; 3]> {
        vec![[0.6, 0.0, 0.0], [0.6, 1.0, 0.0], [0.6, 1.0, 1.0], [0.6, 0.0, 1.0]]
    }

    #[test]
    fn a_second_copy_of_a_panel_with_other_vertices_is_dropped() {
        // The DO27 box: the panel twice, the copy on its own vertices (its UVs differ).
        let mut positions = quad();
        positions.extend(quad());
        let mut indices = vec![0, 1, 2, 0, 2, 3, 4, 5, 6, 4, 6, 7];
        assert_eq!(drop_coincident_triangles(&positions, &mut indices), 2);
        assert_eq!(indices, vec![0, 1, 2, 0, 2, 3], "the first copy stays");
    }

    #[test]
    fn a_rotated_corner_order_is_the_same_triangle() {
        let positions = quad();
        let mut indices = vec![0, 1, 2, 1, 2, 0, 2, 0, 1];
        assert_eq!(drop_coincident_triangles(&positions, &mut indices), 2);
        assert_eq!(indices, vec![0, 1, 2]);
    }

    #[test]
    fn the_opposite_winding_and_a_shifted_copy_are_kept() {
        let mut positions = quad();
        // The same panel 1 mm out: a separate surface.
        positions.extend(quad().into_iter().map(|p| [p[0] + 0.001, p[1], p[2]]));
        let mut indices = vec![0, 1, 2, 0, 2, 1, 4, 5, 6];
        assert_eq!(drop_coincident_triangles(&positions, &mut indices), 0);
        assert_eq!(indices, vec![0, 1, 2, 0, 2, 1, 4, 5, 6]);
    }

    #[test]
    fn negative_zero_matches_zero_and_bad_indices_survive() {
        let positions = vec![
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [-0.0, 0.0, -0.0],
        ];
        let mut indices = vec![0, 1, 2, 3, 1, 2, 0, 9, 2, 0, 9, 2, 7];
        assert_eq!(drop_coincident_triangles(&positions, &mut indices), 1);
        // The out-of-range triangles have no key and are left for the mesh validation to
        // reject; the trailing partial index stays where it was.
        assert_eq!(indices, vec![0, 1, 2, 0, 9, 2, 0, 9, 2, 7]);
    }
}
