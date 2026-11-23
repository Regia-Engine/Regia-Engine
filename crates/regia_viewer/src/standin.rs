// SPDX-License-Identifier: MPL-2.0
//! A small synthetic stand-in piece for tests, benches and the agent's stills: a generic stud made of a
//! ring, a bezel, a post and one round cut stone, with proportions from published ideal-cut figures.
//! Real parts come from part kits.

use glam::{Affine3A, Vec3};

use crate::material::{GemParams, MetalParams, ViewerMaterial};
use crate::mesh::{ViewerBounds, ViewerMeshData, ViewerVertex};

/// One part of a stand-in piece.
#[derive(Debug, Clone, PartialEq)]
pub struct StandInPart {
    /// A readable name.
    pub name: &'static str,
    /// The mesh.
    pub mesh: ViewerMeshData,
    /// The part's material.
    pub material: ViewerMaterial,
    /// Where the part sits.
    pub transforms: Vec<Affine3A>,
}

/// A surface of revolution around +Z with smooth normals. `profile` lists (radius, z) from the back to the
/// front; the outside faces away from the axis.
#[must_use]
pub fn lathe(profile: &[(f32, f32)], segments: u32) -> ViewerMeshData {
    let segments = segments.max(3);
    let rows = profile.len();
    // One ring has no quad. An empty profile used to underflow `rows - 1` in debug.
    if rows < 2 {
        return ViewerMeshData::default();
    }
    let mut vertices = Vec::with_capacity(rows * (segments as usize + 1));
    for (i, &(r, z)) in profile.iter().enumerate() {
        let (prev_r, prev_z) = profile[i.saturating_sub(1)];
        let (next_r, next_z) = profile[(i + 1).min(rows - 1)];
        let tangent = glam::Vec2::new(next_r - prev_r, next_z - prev_z);
        let normal2 = glam::Vec2::new(tangent.y, -tangent.x).normalize_or_zero();
        for s in 0..=segments {
            let angle = s as f32 / segments as f32 * std::f32::consts::TAU;
            let (sin, cos) = angle.sin_cos();
            vertices.push(ViewerVertex {
                position: [r * cos, r * sin, z],
                normal: [normal2.x * cos, normal2.x * sin, normal2.y],
                uv: [s as f32 / segments as f32, i as f32 / (rows - 1).max(1) as f32],
                facet: 0.0,
            });
        }
    }
    let stride = segments + 1;
    let mut indices = Vec::with_capacity((rows - 1) * segments as usize * 6);
    for i in 0..rows as u32 - 1 {
        for s in 0..segments {
            let a = i * stride + s;
            let b = a + 1;
            let c = a + stride;
            let d = c + 1;
            indices.extend_from_slice(&[a, b, d, a, d, c]);
        }
    }
    ViewerMeshData { vertices, indices }
}

/// A sphere of radius `radius` around the origin.
#[must_use]
pub fn ball(radius: f32, segments: u32) -> ViewerMeshData {
    let rows = (segments / 2).max(3);
    let profile: Vec<(f32, f32)> = (0..=rows)
        .map(|i| {
            let angle = -std::f32::consts::FRAC_PI_2 + i as f32 / rows as f32 * std::f32::consts::PI;
            (radius * angle.cos(), radius * angle.sin())
        })
        .collect();
    let mut mesh = lathe(&profile, segments);
    // The poles of a sphere point straight along the axis.
    for v in &mut mesh.vertices {
        v.normal = (Vec3::from(v.position) / radius).normalize_or_zero().to_array();
    }
    mesh
}

/// A round cut as flat facets, with one normal and one number per facet: a table, `fold` star-and-kite
/// crown facets per ring and `fold` pavilion facets. Crown 34.5 degrees, pavilion 40.75 degrees, table 53 %
/// of the girdle diameter (Tolkowsky, *Diamond Design*, 1919). The girdle sits on z = 0, the table faces +Z.
#[must_use]
pub fn round_cut(diameter: f32, fold: u32) -> ViewerMeshData {
    let fold = fold.max(3);
    let girdle = diameter * 0.5;
    let table = girdle * 0.53;
    let crown_height = (girdle - table) * 34.5f32.to_radians().tan();
    let culet_depth = girdle * 40.75f32.to_radians().tan();
    let ring = |radius: f32, z: f32, turn: f32| -> Vec<Vec3> {
        (0..fold)
            .map(|i| {
                let angle = (i as f32 + turn) / fold as f32 * std::f32::consts::TAU;
                Vec3::new(radius * angle.cos(), radius * angle.sin(), z)
            })
            .collect()
    };
    let table_ring = ring(table, crown_height, 0.0);
    let girdle_ring = ring(girdle, 0.0, 0.5);
    let culet = Vec3::new(0.0, 0.0, -culet_depth);
    let table_center = Vec3::new(0.0, 0.0, crown_height);

    let mut mesh = ViewerMeshData::default();
    let mut facet = 1.0;
    let mut add = |points: &[Vec3]| {
        let base = mesh.vertices.len() as u32;
        let normal = (points[1] - points[0])
            .cross(points[2] - points[0])
            .normalize_or_zero();
        for p in points {
            mesh.vertices.push(ViewerVertex {
                position: p.to_array(),
                normal: normal.to_array(),
                uv: [p.x / diameter + 0.5, p.y / diameter + 0.5],
                facet,
            });
        }
        for i in 1..points.len() as u32 - 1 {
            mesh.indices.extend_from_slice(&[base, base + i, base + i + 1]);
        }
        facet += 1.0;
    };
    let mut table_fan = vec![table_center];
    for i in 0..=fold as usize {
        table_fan.push(table_ring[i % fold as usize]);
    }
    add(&table_fan);
    for i in 0..fold as usize {
        let next = (i + 1) % fold as usize;
        let previous = (i + fold as usize - 1) % fold as usize;
        // Crown: one kite from the table edge to the girdle, one triangle in between.
        add(&[table_ring[i], girdle_ring[previous], girdle_ring[i]]);
        add(&[table_ring[i], girdle_ring[i], table_ring[next]]);
        // Pavilion: one long facet per girdle edge, down to the culet.
        add(&[girdle_ring[i], girdle_ring[previous], culet]);
    }
    mesh
}

/// The stand-in stud: 8 mm wide with a 5 mm stone, facing +Z.
#[must_use]
pub fn stud() -> Vec<StandInPart> {
    let gold = ViewerMaterial::Metal(MetalParams {
        base_color: [1.0, 0.77, 0.34],
        roughness: 0.14,
    });
    let stone = ViewerMaterial::Gem(GemParams {
        tint: [1.0, 1.0, 1.0],
        roughness: 0.02,
        ior: 2.42,
        dispersion: 0.5,
        sparkle: 0.6,
        seed: 0.0,
    });
    // A bezel cup: back plate, outer wall, rounded lip, inner wall.
    let bezel = lathe(
        &[
            (0.0, -2.3),
            (2.2, -2.3),
            (2.9, -1.8),
            (3.2, -0.2),
            (3.2, 0.25),
            (3.05, 0.42),
            (2.82, 0.36),
            (2.5, 0.05),
            (2.5, -0.2),
            (0.0, -0.2),
        ],
        64,
    );
    let post = lathe(
        &[(0.0, -9.0), (0.45, -8.9), (0.5, -8.6), (0.5, -2.2), (0.0, -2.2)],
        24,
    );
    let beads = ball(0.42, 20);
    let bead_transforms = (0..12)
        .map(|i| {
            let angle = i as f32 / 12.0 * std::f32::consts::TAU;
            Affine3A::from_translation(Vec3::new(3.72 * angle.cos(), 3.72 * angle.sin(), -0.1))
        })
        .collect();
    vec![
        StandInPart {
            name: "bezel",
            mesh: bezel,
            material: gold,
            transforms: vec![Affine3A::IDENTITY],
        },
        StandInPart {
            name: "post",
            mesh: post,
            material: gold,
            transforms: vec![Affine3A::IDENTITY],
        },
        StandInPart {
            name: "beads",
            mesh: beads,
            material: gold,
            transforms: bead_transforms,
        },
        StandInPart {
            name: "stone",
            mesh: round_cut(5.0, 16),
            material: stone,
            transforms: vec![Affine3A::from_translation(Vec3::new(0.0, 0.0, 0.05))],
        },
    ]
}

/// The box around the stud's visible parts: its post is hidden behind the surface. A contact shadow belongs
/// behind these.
#[must_use]
pub fn stud_visible_bounds() -> ViewerBounds {
    let parts = stud();
    let mut boxes = parts.iter().filter(|part| part.name != "post").flat_map(|part| {
        let bounds = part.mesh.bounds();
        part.transforms.iter().map(move |t| bounds.transformed(t))
    });
    let first = boxes.next().unwrap_or(ViewerBounds {
        min: [0.0; 3],
        max: [0.0; 3],
    });
    boxes.fold(first, |a, b| a.union(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_stand_in_mesh_is_valid_and_faces_out() {
        for part in stud() {
            part.mesh
                .validate()
                .unwrap_or_else(|e| panic!("{}: {e}", part.name));
            // A closed-ish outward mesh has a positive signed volume.
            let mut volume = 0.0;
            for tri in part.mesh.indices.as_chunks::<3>().0 {
                let a = Vec3::from(part.mesh.vertices[tri[0] as usize].position);
                let b = Vec3::from(part.mesh.vertices[tri[1] as usize].position);
                let c = Vec3::from(part.mesh.vertices[tri[2] as usize].position);
                volume += a.dot(b.cross(c)) / 6.0;
            }
            assert!(volume > 0.0, "{} volume {volume}", part.name);
        }
    }

    #[test]
    fn the_visible_bounds_leave_out_the_post() {
        let bounds = stud_visible_bounds();
        // The back of the bezel cup is the deepest visible point; the post reaches -9.
        assert!((bounds.min[2] + 2.3).abs() < 1e-5, "{bounds:?}");
        assert!(bounds.max[0] > 4.0, "the beads are visible: {bounds:?}");
    }

    #[test]
    fn the_stone_stays_inside_the_bezel_cup() {
        let parts = stud();
        let z_min = |name: &str| {
            let part = parts.iter().find(|p| p.name == name).expect("part");
            part.mesh.bounds().transformed(&part.transforms[0]).min[2]
        };
        assert!(
            z_min("stone") > z_min("bezel"),
            "the culet breaks through the back"
        );
    }

    #[test]
    fn an_empty_lathe_profile_is_an_empty_mesh() {
        let mesh = lathe(&[], 16);
        assert!(mesh.vertices.is_empty() && mesh.indices.is_empty());
    }

    #[test]
    fn a_facet_has_one_normal_and_one_number() {
        let cut = round_cut(5.0, 8);
        let mut seen = std::collections::BTreeMap::new();
        for v in &cut.vertices {
            let key = v.facet as u32;
            let normal = *seen.entry(key).or_insert(v.normal);
            assert_eq!(normal, v.normal, "facet {key}");
        }
        // Table + 3 per fold.
        assert_eq!(seen.len(), 1 + 3 * 8);
    }
}
