// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from Bevy (https://github.com/bevyengine/bevy, commit fd98063),
// crates/bevy_mesh/src/primitives/dim3/capsule.rs. Copyright (c) Bevy contributors.
// Bevy is dual-licensed MIT OR Apache-2.0; this file uses it under the MIT licence:
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.

//! Capsule mesher: two hemispheres joined by an optional subdivided cylinder, axis along Y.
//!
//! Bevy adapted it from Jeremy Behreandt's "Making a capsule mesh via script in five 3D
//! environments" (<https://behreajj.medium.com/making-a-capsule-mesh-via-script-in-five-3d-environments-c2214abf02db>).
//! Regia Engine's changes: `latitudes` is clamped to an even number ≥ 4 and `longitudes` to ≥ 3 (Bevy
//! underflows below that), and indices are pushed instead of preallocated.

use regia_core::math::{Vec2, Vec3};

use super::MeshParts;
use crate::runtime::CpuMesh;

/// How a [`Capsule`]'s UV `v` range is split between the hemispheres and the cylinder.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CapsuleUvProfile {
    /// Space proportional to how much of the capsule's height each part covers.
    #[default]
    Aspect,
    /// Hemispheres get space by the ratio of latitudes to rings.
    Uniform,
    /// A third each: north hemisphere, cylinder, south hemisphere.
    Fixed,
}

/// A capsule centred at the origin with its axis along Y.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Capsule {
    /// Radius of the hemispheres and the cylinder, metres.
    pub radius: f32,
    /// Half the distance between the hemisphere centres, metres (total height is
    /// `2 × (half_length + radius)`).
    pub half_length: f32,
    /// Extra vertex rings subdividing the cylinder.
    pub rings: u32,
    /// Vertex columns around the axis (at least 3).
    pub longitudes: u32,
    /// Rows over both hemispheres together (rounded up to an even number, at least 4).
    pub latitudes: u32,
    /// UV `v` distribution.
    pub uv_profile: CapsuleUvProfile,
}

impl Default for Capsule {
    fn default() -> Self {
        Self {
            radius: 0.5,
            half_length: 0.5,
            rings: 0,
            longitudes: 32,
            latitudes: 16,
            uv_profile: CapsuleUvProfile::Aspect,
        }
    }
}

impl Capsule {
    /// Clamped `(longitudes, latitudes)` the mesher actually uses.
    #[must_use]
    pub fn resolution(&self) -> (u32, u32) {
        let latitudes = self.latitudes.max(4);
        (self.longitudes.max(3), latitudes + latitudes % 2)
    }

    /// Triangle count: `2 × longitudes × (latitudes + rings)` with the clamped resolution.
    #[must_use]
    pub fn triangle_count(&self) -> usize {
        let (longitudes, latitudes) = self.resolution();
        (2 * longitudes * (latitudes + self.rings)) as usize
    }

    /// Builds the mesh.
    #[must_use]
    pub fn build(&self) -> CpuMesh {
        let (longitudes, latitudes) = self.resolution();
        let rings = self.rings;
        let radius = self.radius.abs();
        let half_length = self.half_length.abs();

        let calc_middle = rings > 0;
        let half_lats = latitudes / 2;
        let half_latsn1 = half_lats - 1;
        let half_latsn2 = half_lats - 2;
        let ringsp1 = rings + 1;
        let lonsp1 = longitudes + 1;
        let summit = half_length + radius;

        // Vertex index offsets.
        let vert_offset_north_hemi = longitudes;
        let vert_offset_north_equator = vert_offset_north_hemi + lonsp1 * half_latsn1;
        let vert_offset_cylinder = vert_offset_north_equator + lonsp1;
        let vert_offset_south_equator = if calc_middle {
            vert_offset_cylinder + lonsp1 * rings
        } else {
            vert_offset_cylinder
        };
        let vert_offset_south_hemi = vert_offset_south_equator + lonsp1;
        let vert_offset_south_polar = vert_offset_south_hemi + lonsp1 * half_latsn2;
        let vert_offset_south_cap = vert_offset_south_polar + lonsp1;

        let vert_len = (vert_offset_south_cap + longitudes) as usize;
        let mut vs: Vec<Vec3> = vec![Vec3::ZERO; vert_len];
        let mut vts: Vec<Vec2> = vec![Vec2::ZERO; vert_len];
        let mut vns: Vec<Vec3> = vec![Vec3::ZERO; vert_len];

        let to_theta = 2.0 * std::f32::consts::PI / longitudes as f32;
        let to_phi = std::f32::consts::PI / latitudes as f32;
        let to_tex_horizontal = 1.0 / longitudes as f32;
        let to_tex_vertical = 1.0 / half_lats as f32;

        let vt_aspect_ratio = match self.uv_profile {
            CapsuleUvProfile::Aspect => {
                let total = 2.0 * half_length + radius + radius;
                if total > 0.0 { radius / total } else { 0.5 }
            }
            CapsuleUvProfile::Uniform => half_lats as f32 / (ringsp1 + latitudes) as f32,
            CapsuleUvProfile::Fixed => 1.0 / 3.0,
        };
        let vt_aspect_north = 1.0 - vt_aspect_ratio;
        let vt_aspect_south = vt_aspect_ratio;

        let mut theta_cartesian: Vec<Vec2> = vec![Vec2::ZERO; longitudes as usize];
        let mut rho_theta_cartesian: Vec<Vec2> = vec![Vec2::ZERO; longitudes as usize];
        let mut s_texture_cache: Vec<f32> = vec![0.0; lonsp1 as usize];

        for j in 0..longitudes as usize {
            let jf = j as f32;
            let s_texture_polar = 1.0 - ((jf + 0.5) * to_tex_horizontal);
            let theta = jf * to_theta;

            theta_cartesian[j] = Vec2::from_angle(theta);
            rho_theta_cartesian[j] = radius * theta_cartesian[j];

            // North.
            vs[j] = Vec3::new(0.0, summit, 0.0);
            vts[j] = Vec2::new(s_texture_polar, 1.0);
            vns[j] = Vec3::Y;

            // South.
            let idx = vert_offset_south_cap as usize + j;
            vs[idx] = Vec3::new(0.0, -summit, 0.0);
            vts[idx] = Vec2::new(s_texture_polar, 0.0);
            vns[idx] = Vec3::NEG_Y;
        }

        // Equatorial vertices.
        for (j, s_texture_cache_j) in s_texture_cache.iter_mut().enumerate() {
            let s_texture = 1.0 - j as f32 * to_tex_horizontal;
            *s_texture_cache_j = s_texture;

            // Wrap to the first element on the last.
            let j_mod = j % longitudes as usize;
            let tc = theta_cartesian[j_mod];
            let rtc = rho_theta_cartesian[j_mod];

            let idxn = vert_offset_north_equator as usize + j;
            vs[idxn] = Vec3::new(rtc.x, half_length, -rtc.y);
            vts[idxn] = Vec2::new(s_texture, vt_aspect_north);
            vns[idxn] = Vec3::new(tc.x, 0.0, -tc.y);

            let idxs = vert_offset_south_equator as usize + j;
            vs[idxs] = Vec3::new(rtc.x, -half_length, -rtc.y);
            vts[idxs] = Vec2::new(s_texture, vt_aspect_south);
            vns[idxs] = Vec3::new(tc.x, 0.0, -tc.y);
        }

        // Hemisphere vertices.
        for i in 0..half_latsn1 {
            let ip1f = i as f32 + 1.0;
            let phi = ip1f * to_phi;

            // Symmetrical hemispheres: one sine/cosine pair serves both.
            let (sin_phi_south, cos_phi_south) = phi.sin_cos();
            let cos_phi_north = sin_phi_south;
            let sin_phi_north = -cos_phi_south;

            let rho_cos_phi_north = radius * cos_phi_north;
            let rho_sin_phi_north = radius * sin_phi_north;
            let z_offset_north = half_length - rho_sin_phi_north;

            let rho_cos_phi_south = radius * cos_phi_south;
            let rho_sin_phi_south = radius * sin_phi_south;
            let z_offset_south = -half_length - rho_sin_phi_south;

            let t_tex_fac = ip1f * to_tex_vertical;
            let cmpl_tex_fac = 1.0 - t_tex_fac;
            let t_tex_north = cmpl_tex_fac + vt_aspect_north * t_tex_fac;
            let t_tex_south = cmpl_tex_fac * vt_aspect_south;

            let i_lonsp1 = i * lonsp1;
            let vert_curr_lat_north = vert_offset_north_hemi + i_lonsp1;
            let vert_curr_lat_south = vert_offset_south_hemi + i_lonsp1;

            for (j, s_texture) in s_texture_cache.iter().enumerate() {
                let j_mod = j % longitudes as usize;
                let tc = theta_cartesian[j_mod];

                let idxn = vert_curr_lat_north as usize + j;
                vs[idxn] = Vec3::new(
                    rho_cos_phi_north * tc.x,
                    z_offset_north,
                    -rho_cos_phi_north * tc.y,
                );
                vts[idxn] = Vec2::new(*s_texture, t_tex_north);
                vns[idxn] = Vec3::new(cos_phi_north * tc.x, -sin_phi_north, -cos_phi_north * tc.y);

                let idxs = vert_curr_lat_south as usize + j;
                vs[idxs] = Vec3::new(
                    rho_cos_phi_south * tc.x,
                    z_offset_south,
                    -rho_cos_phi_south * tc.y,
                );
                vts[idxs] = Vec2::new(*s_texture, t_tex_south);
                vns[idxs] = Vec3::new(cos_phi_south * tc.x, -sin_phi_south, -cos_phi_south * tc.y);
            }
        }

        // Cylinder vertices (the equators are excluded from the interpolation).
        if calc_middle {
            let to_fac = 1.0 / ringsp1 as f32;
            let mut idx_cyl_lat = vert_offset_cylinder as usize;
            for h in 1..ringsp1 {
                let fac = h as f32 * to_fac;
                let cmpl_fac = 1.0 - fac;
                let t_texture = cmpl_fac * vt_aspect_north + fac * vt_aspect_south;
                let z = half_length - 2.0 * half_length * fac;
                for (j, s_texture) in s_texture_cache.iter().enumerate() {
                    let j_mod = j % longitudes as usize;
                    let tc = theta_cartesian[j_mod];
                    let rtc = rho_theta_cartesian[j_mod];
                    vs[idx_cyl_lat] = Vec3::new(rtc.x, z, -rtc.y);
                    vts[idx_cyl_lat] = Vec2::new(*s_texture, t_texture);
                    vns[idx_cyl_lat] = Vec3::new(tc.x, 0.0, -tc.y);
                    idx_cyl_lat += 1;
                }
            }
        }

        let mut parts = MeshParts::with_capacity(vert_len, self.triangle_count() * 3);
        for ((p, n), t) in vs.iter().zip(&vns).zip(&vts) {
            parts.push(p.to_array(), n.to_array(), t.to_array());
        }
        let tris = &mut parts.indices;

        // Polar caps (north, then south at the end, as Bevy orders them).
        for i in 0..longitudes {
            tris.extend_from_slice(&[i, vert_offset_north_hemi + i, vert_offset_north_hemi + i + 1]);
        }
        let mut south_cap = Vec::with_capacity(longitudes as usize * 3);
        for i in 0..longitudes {
            south_cap.extend_from_slice(&[
                vert_offset_south_cap + i,
                vert_offset_south_polar + i + 1,
                vert_offset_south_polar + i,
            ]);
        }

        // Hemispheres.
        let mut south_hemi = Vec::with_capacity((half_latsn1 * longitudes * 6) as usize);
        for i in 0..half_latsn1 {
            let i_lonsp1 = i * lonsp1;
            let vert_curr_lat_north = vert_offset_north_hemi + i_lonsp1;
            let vert_next_lat_north = vert_curr_lat_north + lonsp1;
            let vert_curr_lat_south = vert_offset_south_equator + i_lonsp1;
            let vert_next_lat_south = vert_curr_lat_south + lonsp1;
            for j in 0..longitudes {
                let north00 = vert_curr_lat_north + j;
                let north01 = vert_next_lat_north + j;
                let north11 = vert_next_lat_north + j + 1;
                let north10 = vert_curr_lat_north + j + 1;
                tris.extend_from_slice(&[north00, north11, north10, north00, north01, north11]);

                let south00 = vert_curr_lat_south + j;
                let south01 = vert_next_lat_south + j;
                let south11 = vert_next_lat_south + j + 1;
                let south10 = vert_curr_lat_south + j + 1;
                south_hemi.extend_from_slice(&[south00, south11, south10, south00, south01, south11]);
            }
        }

        // Cylinder.
        for i in 0..ringsp1 {
            let vert_curr_lat = vert_offset_north_equator + i * lonsp1;
            let vert_next_lat = vert_curr_lat + lonsp1;
            for j in 0..longitudes {
                let cy00 = vert_curr_lat + j;
                let cy01 = vert_next_lat + j;
                let cy11 = vert_next_lat + j + 1;
                let cy10 = vert_curr_lat + j + 1;
                tris.extend_from_slice(&[cy00, cy11, cy10, cy00, cy01, cy11]);
            }
        }
        tris.extend_from_slice(&south_hemi);
        tris.extend_from_slice(&south_cap);
        parts.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::primitives::check;

    #[test]
    fn capsule_is_closed_and_counts_match() {
        for (rings, longitudes, latitudes) in [(0, 3, 4), (0, 32, 16), (3, 12, 8), (1, 5, 7), (0, 1, 1)] {
            let capsule = Capsule {
                radius: 0.4,
                half_length: 0.6,
                rings,
                longitudes,
                latitudes,
                uv_profile: CapsuleUvProfile::Aspect,
            };
            let mesh = capsule.build();
            let what = format!("capsule r{rings} lon{longitudes} lat{latitudes}");
            check::assert_well_formed(&mesh, &what);
            check::assert_watertight(&mesh, &what);
            assert_eq!(check::triangles(&mesh), capsule.triangle_count(), "{what}");
            let top = mesh
                .vertices
                .iter()
                .map(|v| v.position[1])
                .fold(f32::MIN, f32::max);
            assert!((top - 1.0).abs() < 1.0e-5, "{what}: summit {top}");
        }
    }

    #[test]
    fn capsule_uv_profiles_stay_in_range() {
        for uv_profile in [
            CapsuleUvProfile::Aspect,
            CapsuleUvProfile::Uniform,
            CapsuleUvProfile::Fixed,
        ] {
            let mesh = Capsule {
                rings: 2,
                uv_profile,
                ..Capsule::default()
            }
            .build();
            check::assert_well_formed(&mesh, "capsule uv");
        }
    }
}
