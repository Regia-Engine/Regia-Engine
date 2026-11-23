// SPDX-License-Identifier: MPL-2.0
//! Small meshes for the looks: flat-shaded gem facets and smooth generic parts (ring band, ball,
//! cabochon dome, bezel cup), all in millimeters with +z toward the viewer's default front.

use bytemuck::{Pod, Zeroable};
use glam::Vec3;

/// One vertex as the look shaders read it: position, normal, facet id (as a float, so the
/// WebGL2 path needs no integer attributes).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
pub struct LookVertex {
    /// Position in millimeters.
    pub position: [f32; 3],
    /// Unit normal (the facet normal on gems).
    pub normal: [f32; 3],
    /// Facet id (0 on smooth meshes); the gem shader hashes it into a per-facet sparkle phase.
    pub facet: f32,
}

/// An indexed triangle mesh.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LookMesh {
    /// Positions in millimeters.
    pub positions: Vec<[f32; 3]>,
    /// Unit normals.
    pub normals: Vec<[f32; 3]>,
    /// Facet id per vertex.
    pub facets: Vec<u32>,
    /// Triangle list, counter-clockwise seen from outside.
    pub indices: Vec<u32>,
}

impl LookMesh {
    /// Appends a convex polygon as a triangle fan with one shared flat normal.
    pub fn push_flat_polygon(&mut self, vertices: &[Vec3], normal: Vec3, facet: u32) {
        let base = self.positions.len() as u32;
        for v in vertices {
            self.positions.push(v.to_array());
            self.normals.push(normal.to_array());
            self.facets.push(facet);
        }
        for k in 1..vertices.len().saturating_sub(1) as u32 {
            self.indices.extend_from_slice(&[base, base + k, base + k + 1]);
        }
    }

    /// Number of triangles.
    pub fn triangle_count(&self) -> usize {
        self.indices.len() / 3
    }

    /// Interleaved vertices for upload.
    pub fn vertices(&self) -> Vec<LookVertex> {
        (0..self.positions.len())
            .map(|i| LookVertex {
                position: self.positions[i],
                normal: self.normals[i],
                facet: self.facets[i] as f32,
            })
            .collect()
    }

    /// Signed volume by the divergence theorem (positive for a closed, outward-wound mesh).
    pub fn signed_volume(&self) -> f32 {
        let p = |i: u32| Vec3::from(self.positions[i as usize]);
        self.indices
            .chunks(3)
            .map(|t| p(t[0]).dot(p(t[1]).cross(p(t[2]))) / 6.0)
            .sum()
    }

    /// Axis-aligned bounds `(min, max)`; zero for an empty mesh.
    pub fn bounds(&self) -> (Vec3, Vec3) {
        if self.positions.is_empty() {
            return (Vec3::ZERO, Vec3::ZERO);
        }
        self.positions
            .iter()
            .fold((Vec3::splat(f32::MAX), Vec3::splat(f32::MIN)), |(lo, hi), p| {
                (lo.min(Vec3::from(*p)), hi.max(Vec3::from(*p)))
            })
    }

    /// Appends another mesh (keeping its facet ids offset past this mesh's).
    pub fn append(&mut self, other: &LookMesh) {
        let base = self.positions.len() as u32;
        let facet_base = self.facets.iter().copied().max().map_or(0, |m| m + 1);
        self.positions.extend_from_slice(&other.positions);
        self.normals.extend_from_slice(&other.normals);
        self.facets.extend(other.facets.iter().map(|f| f + facet_base));
        self.indices.extend(other.indices.iter().map(|i| i + base));
    }

    /// A smooth surface of revolution about +z from a profile of `(radius, z, normal_r, normal_z)`
    /// points, `segments` around. Zero segments, a profile of fewer than two points or a
    /// non-finite profile value (a zero ring or side count divides by zero) gives an empty mesh:
    /// before, `segments == 0` gave a ring of NaN positions (corr-looks LOW).
    fn revolve(profile: &[(f32, f32, f32, f32)], segments: u32) -> LookMesh {
        let mut mesh = LookMesh::default();
        let finite = profile
            .iter()
            .all(|&(r, z, nr, nz)| r.is_finite() && z.is_finite() && nr.is_finite() && nz.is_finite());
        if segments == 0 || profile.len() < 2 || !finite {
            return mesh;
        }
        let rings = profile.len() as u32;
        for s in 0..=segments {
            let a = s as f32 / segments as f32 * std::f32::consts::TAU;
            let (sa, ca) = a.sin_cos();
            for &(r, z, nr, nz) in profile {
                mesh.positions.push([r * ca, r * sa, z]);
                mesh.normals
                    .push(Vec3::new(nr * ca, nr * sa, nz).normalize_or_zero().to_array());
                mesh.facets.push(0);
            }
        }
        for s in 0..segments {
            for k in 0..rings - 1 {
                let a = s * rings + k;
                let b = (s + 1) * rings + k;
                // The profile runs from bottom to top; this winding faces outward.
                mesh.indices.extend_from_slice(&[a, b, a + 1, a + 1, b, b + 1]);
            }
        }
        mesh
    }

    /// A ring band (torus) of outer diameter `outer_mm` and wire diameter `wire_mm`, axis along +z
    /// (seen face-on from the default front).
    pub fn ring_band(outer_mm: f32, wire_mm: f32, segments: u32, sides: u32) -> LookMesh {
        let r_wire = wire_mm * 0.5;
        let r_major = outer_mm * 0.5 - r_wire;
        let profile: Vec<(f32, f32, f32, f32)> = (0..=sides)
            .map(|k| {
                // From the bottom (-pi/2) around the outside to the top and back inside.
                let t = -std::f32::consts::FRAC_PI_2 + k as f32 / sides as f32 * std::f32::consts::TAU;
                let (st, ct) = t.sin_cos();
                (r_major + r_wire * ct, r_wire * st, ct, st)
            })
            .collect();
        Self::revolve(&profile, segments)
    }

    /// A ball (bead) of diameter `d_mm`.
    pub fn ball(d_mm: f32, segments: u32, rings: u32) -> LookMesh {
        let r = d_mm * 0.5;
        let profile: Vec<(f32, f32, f32, f32)> = (0..=rings)
            .map(|k| {
                let t = -std::f32::consts::FRAC_PI_2 + k as f32 / rings as f32 * std::f32::consts::PI;
                let (st, ct) = t.sin_cos();
                (r * ct, r * st, ct, st)
            })
            .collect();
        Self::revolve(&profile, segments)
    }

    /// A round cabochon: a flat base of diameter `d_mm` and a dome of height `h_mm` (an ellipsoid cap).
    pub fn cabochon(d_mm: f32, h_mm: f32, segments: u32, rings: u32) -> LookMesh {
        let r = d_mm * 0.5;
        let mut profile = vec![(0.0, 0.0, 0.0, -1.0), (r, 0.0, 0.0, -1.0)];
        for k in 0..=rings {
            let t = k as f32 / rings as f32 * std::f32::consts::FRAC_PI_2;
            let (st, ct) = t.sin_cos();
            // Ellipsoid normal: (x / r^2, z / h^2).
            profile.push((r * ct, h_mm * st, ct / r, st / h_mm.max(1e-4)));
        }
        Self::revolve(&profile, segments)
    }

    /// A bezel cup: a short tube of outer diameter `outer_mm`, wall `wall_mm`, height `h_mm`, with a
    /// floor, open at the top. The stone sits inside, its girdle at the top lip.
    pub fn bezel_cup(outer_mm: f32, wall_mm: f32, h_mm: f32, segments: u32) -> LookMesh {
        let ro = outer_mm * 0.5;
        let ri = ro - wall_mm;
        let lip = wall_mm * 0.5;
        let mut profile = vec![(0.0, -h_mm, 0.0, -1.0), (ro - lip, -h_mm, 0.0, -1.0)];
        // Rounded bottom edge, outer wall, rounded lip, inner wall.
        for k in 0..=4 {
            let t = -std::f32::consts::FRAC_PI_2 + k as f32 / 4.0 * std::f32::consts::FRAC_PI_2;
            let (st, ct) = t.sin_cos();
            profile.push((ro - lip + lip * ct, -h_mm + lip + lip * st, ct, st));
        }
        for k in 0..=6 {
            let t = k as f32 / 6.0 * std::f32::consts::PI;
            let (st, ct) = t.sin_cos();
            profile.push((ri + lip + lip * ct, -lip + lip * st, ct, st));
        }
        profile.push((ri, -h_mm * 0.6, -1.0, 0.0));
        profile.push((ri, -h_mm * 0.6, 0.0, 1.0));
        profile.push((0.0, -h_mm * 0.6, 0.0, 1.0));
        Self::revolve(&profile, segments)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A zero segment, side or ring count (or a NaN size) builds an empty mesh, never NaN
    /// positions (corr-looks LOW: `revolve` divided by a zero `segments`).
    #[test]
    fn zero_counts_build_an_empty_mesh() {
        for mesh in [
            LookMesh::ring_band(18.0, 2.0, 0, 12),
            LookMesh::ring_band(18.0, 2.0, 32, 0),
            LookMesh::ball(4.0, 0, 8),
            LookMesh::ball(4.0, 16, 0),
            LookMesh::cabochon(6.0, 2.0, 0, 6),
            LookMesh::cabochon(6.0, 2.0, 24, 0),
            LookMesh::bezel_cup(6.0, 0.5, 2.0, 0),
            LookMesh::ball(f32::NAN, 16, 8),
        ] {
            assert!(
                mesh.positions.iter().flatten().all(|v| v.is_finite()),
                "NaN positions"
            );
            assert!(mesh.positions.is_empty() && mesh.indices.is_empty());
        }
        let ball = LookMesh::ball(4.0, 16, 8);
        assert!(ball.triangle_count() > 0 && ball.signed_volume() > 0.0);
    }

    #[test]
    fn ball_volume_matches_sphere() {
        let m = LookMesh::ball(2.0, 64, 32);
        let exact = 4.0 / 3.0 * std::f32::consts::PI;
        assert!(
            (m.signed_volume() - exact).abs() < 0.02 * exact,
            "{}",
            m.signed_volume()
        );
    }

    #[test]
    fn ring_band_volume_matches_torus() {
        let m = LookMesh::ring_band(20.0, 2.0, 96, 32);
        // Torus volume 2 pi^2 R r^2 with R = 9, r = 1.
        let exact = 2.0 * std::f32::consts::PI.powi(2) * 9.0;
        assert!(
            (m.signed_volume() - exact).abs() < 0.02 * exact,
            "{}",
            m.signed_volume()
        );
    }

    #[test]
    fn cabochon_and_bezel_are_outward() {
        assert!(LookMesh::cabochon(6.0, 2.0, 48, 16).signed_volume() > 0.0);
        let (lo, hi) = LookMesh::bezel_cup(5.0, 0.4, 1.5, 48).bounds();
        assert!((hi.x - lo.x - 5.0).abs() < 0.05);
    }

    #[test]
    fn append_offsets_indices_and_facets() {
        let mut a = LookMesh::default();
        a.push_flat_polygon(&[Vec3::ZERO, Vec3::X, Vec3::Y], Vec3::Z, 3);
        let b = a.clone();
        a.append(&b);
        assert_eq!(a.indices, vec![0, 1, 2, 3, 4, 5]);
        assert_eq!(a.facets, vec![3, 3, 3, 7, 7, 7]);
    }
}
