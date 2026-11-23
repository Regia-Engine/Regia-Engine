// SPDX-License-Identifier: MPL-2.0
//! Closed-mesh detection for the glTF cook hint `drop_double_sided_on_closed`
//! (`GltfImporterSettings` in a glTF `.meta`).
//!
//! A glTF `doubleSided` material draws both faces of every triangle. On a mesh that is a closed,
//! consistently wound, outward-facing surface with nothing see-through in it, the back faces are
//! only visible from inside the volume, so from outside the flag changes nothing on screen while
//! it costs the cull-none draws. The hint drops the flag for materials used only by such meshes.
//! It is off by default: a camera inside the volume (a cockpit inside a hull) sees the culled
//! inside.
//!
//! The test is the oriented 2-manifold condition (each directed edge once, its reverse once:
//! Botsch et al., *Polygon Mesh Processing*, 2010, § 1.3) on positions welded bit-exactly (so a UV
//! or normal seam does not open the surface), plus a positive signed volume (the divergence
//! theorem over the triangles: outward faces under glTF's counter-clockwise front face). Exact
//! welding is conservative: a near-miss seam keeps the mesh open, and the flag stays.

use std::collections::HashMap;
use std::path::Path;

use crate::ImportError;

/// What the cook hint does with one glTF material.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MaterialSidedness {
    /// Not `doubleSided`: unchanged.
    SingleSided,
    /// `doubleSided` and used by at least one mesh that is open, inside-out or see-through: kept.
    DoubleSidedVisible,
    /// `doubleSided` and used only by closed, opaque, outward-facing meshes: the hint drops the
    /// flag.
    DoubleSidedClosed,
    /// `doubleSided` but no primitive uses it: kept.
    Unused,
}

/// Closed-mesh analysis of one glTF document.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClosedMeshReport {
    /// glTF meshes.
    pub meshes: usize,
    /// Meshes that are closed, consistently wound and outward-facing.
    pub closed_meshes: usize,
    /// Of those, the ones with a see-through primitive (blend, mask or transmission): not eligible.
    pub closed_see_through: usize,
    /// Meshes with at least one `doubleSided` primitive.
    pub meshes_double_sided: usize,
    /// Meshes whose `doubleSided` primitives would all become single-sided under the hint.
    pub meshes_dropped: usize,
    /// Per glTF material, in document order.
    pub materials: Vec<MaterialSidedness>,
}

impl ClosedMeshReport {
    /// Materials whose flag the hint drops.
    #[must_use]
    pub fn dropped_materials(&self) -> usize {
        self.materials
            .iter()
            .filter(|m| **m == MaterialSidedness::DoubleSidedClosed)
            .count()
    }

    /// `doubleSided` materials.
    #[must_use]
    pub fn double_sided_materials(&self) -> usize {
        self.materials
            .iter()
            .filter(|m| **m != MaterialSidedness::SingleSided)
            .count()
    }
}

/// Shape of one mesh's surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MeshShape {
    closed: bool,
    see_through: bool,
}

/// Every directed edge once and its reverse once, over positions welded bit-exactly, with a
/// positive signed volume. `triangles` index `positions`.
fn closed_outward(positions: &[[f32; 3]], triangles: &[[u32; 3]]) -> bool {
    // Bit-exact weld (`-0.0` folded onto `0.0`).
    let bits = |p: [f32; 3]| p.map(|c| if c == 0.0 { 0u32 } else { c.to_bits() });
    let mut weld: HashMap<[u32; 3], u32> = HashMap::with_capacity(positions.len());
    let ids: Vec<u32> = positions
        .iter()
        .map(|&p| {
            let next = weld.len() as u32;
            *weld.entry(bits(p)).or_insert(next)
        })
        .collect();
    let mut edges: HashMap<(u32, u32), u32> = HashMap::with_capacity(triangles.len() * 3);
    let mut volume = 0.0f64;
    for tri in triangles {
        let Some(w) = tri
            .iter()
            .map(|&i| ids.get(i as usize).copied())
            .collect::<Option<Vec<u32>>>()
        else {
            return false;
        };
        if w[0] == w[1] || w[1] == w[2] || w[2] == w[0] {
            // Degenerate: no area, no edges.
            continue;
        }
        for k in 0..3 {
            *edges.entry((w[k], w[(k + 1) % 3])).or_insert(0) += 1;
        }
        let p = tri.map(|i| positions[i as usize].map(f64::from));
        let cross = [
            p[1][1] * p[2][2] - p[1][2] * p[2][1],
            p[1][2] * p[2][0] - p[1][0] * p[2][2],
            p[1][0] * p[2][1] - p[1][1] * p[2][0],
        ];
        volume += (p[0][0] * cross[0] + p[0][1] * cross[1] + p[0][2] * cross[2]) / 6.0;
    }
    !edges.is_empty()
        && edges
            .iter()
            .all(|(&(a, b), &count)| count == 1 && edges.get(&(b, a)) == Some(&1))
        && volume > 0.0
}

/// A material a viewer can see through to the inside of a closed mesh.
fn see_through(material: &gltf::Material<'_>) -> bool {
    material.alpha_mode() != gltf::material::AlphaMode::Opaque
        || material
            .transmission()
            .is_some_and(|t| t.transmission_factor() > 0.0)
}

fn mesh_shape(mesh: &gltf::Mesh<'_>, buffers: &[Vec<u8>]) -> MeshShape {
    let mut positions: Vec<[f32; 3]> = Vec::new();
    let mut triangles: Vec<[u32; 3]> = Vec::new();
    let mut triangles_only = true;
    let mut see = false;
    for primitive in mesh.primitives() {
        see |= see_through(&primitive.material());
        if primitive.mode() != gltf::mesh::Mode::Triangles {
            triangles_only = false;
            continue;
        }
        let reader = primitive.reader(|buffer| buffers.get(buffer.index()).map(Vec::as_slice));
        let Some(read) = reader.read_positions() else {
            triangles_only = false;
            continue;
        };
        let base = positions.len() as u32;
        positions.extend(read);
        let count = positions.len() as u32 - base;
        let indices: Vec<u32> = match reader.read_indices() {
            Some(indices) => indices.into_u32().collect(),
            None => (0..count).collect(),
        };
        triangles.extend(
            indices
                .as_chunks::<3>()
                .0
                .iter()
                .map(|t| t.map(|i| base.saturating_add(i))),
        );
    }
    MeshShape {
        closed: triangles_only && closed_outward(&positions, &triangles),
        see_through: see,
    }
}

/// Analyses every mesh of `document` (`buffers` in document order) and decides each material.
#[must_use]
pub fn closed_mesh_report(document: &gltf::Document, buffers: &[Vec<u8>]) -> ClosedMeshReport {
    let material_count = document.materials().len();
    // Per material: used at all, and used by a mesh that is not eligible.
    let mut used = vec![false; material_count];
    let mut visible = vec![false; material_count];
    let mut report = ClosedMeshReport::default();
    let mut shapes = Vec::new();
    for mesh in document.meshes() {
        let shape = mesh_shape(&mesh, buffers);
        report.meshes += 1;
        report.closed_meshes += usize::from(shape.closed);
        report.closed_see_through += usize::from(shape.closed && shape.see_through);
        let eligible = shape.closed && !shape.see_through;
        for primitive in mesh.primitives() {
            if let Some(index) = primitive.material().index() {
                used[index] = true;
                visible[index] |= !eligible;
            }
        }
        shapes.push(eligible);
    }
    report.materials = document
        .materials()
        .map(|material| {
            let index = material.index().unwrap_or(usize::MAX);
            if !material.double_sided() {
                MaterialSidedness::SingleSided
            } else if !used.get(index).copied().unwrap_or(false) {
                MaterialSidedness::Unused
            } else if visible.get(index).copied().unwrap_or(true) {
                MaterialSidedness::DoubleSidedVisible
            } else {
                MaterialSidedness::DoubleSidedClosed
            }
        })
        .collect();
    for (mesh, eligible) in document.meshes().zip(shapes) {
        let double_sided: Vec<usize> = mesh
            .primitives()
            .filter_map(|p| p.material().index().filter(|_| p.material().double_sided()))
            .collect();
        if double_sided.is_empty() {
            continue;
        }
        report.meshes_double_sided += 1;
        if eligible
            && double_sided
                .iter()
                .all(|&i| report.materials.get(i) == Some(&MaterialSidedness::DoubleSidedClosed))
        {
            report.meshes_dropped += 1;
        }
    }
    report
}

/// [`closed_mesh_report`] of a `.gltf` / `.glb` on disk. Reads the document and its buffers, not
/// its images.
///
/// # Errors
///
/// The file is not a readable glTF or a buffer is missing.
pub fn closed_mesh_report_path(source: &Path) -> Result<ClosedMeshReport, ImportError> {
    let opened = crate::gltf_import::open_gltf(source)?;
    let buffers: Vec<Vec<u8>> = opened.buffers.into_iter().map(|b| b.0).collect();
    Ok(closed_mesh_report(&opened.document, &buffers))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unit cube, outward counter-clockwise faces, each face its own four vertices (split
    /// normals/UVs, as exporters write it).
    fn cube() -> (Vec<[f32; 3]>, Vec<[u32; 3]>) {
        let faces: [([f32; 3], [f32; 3], [f32; 3]); 6] = [
            ([1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]),
            ([-1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0, 0.0]),
            ([0.0, 1.0, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0, 0.0]),
            ([0.0, -1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]),
            ([0.0, 0.0, 1.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
            ([0.0, 0.0, -1.0], [0.0, 1.0, 0.0], [1.0, 0.0, 0.0]),
        ];
        let mut positions = Vec::new();
        let mut triangles = Vec::new();
        for (n, u, v) in faces {
            let base = positions.len() as u32;
            for (a, b) in [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
                positions.push([0, 1, 2].map(|k| 0.5 * n[k] + 0.5 * a * u[k] + 0.5 * b * v[k]));
            }
            triangles.push([base, base + 1, base + 2]);
            triangles.push([base, base + 2, base + 3]);
        }
        (positions, triangles)
    }

    #[test]
    fn a_split_vertex_cube_is_closed_and_outward() {
        let (positions, triangles) = cube();
        assert!(closed_outward(&positions, &triangles));
    }

    #[test]
    fn open_inside_out_or_inconsistent_surfaces_are_not_closed() {
        let (positions, triangles) = cube();
        // One face missing: open.
        assert!(!closed_outward(&positions, &triangles[2..]));
        // Every face reversed: closed but inside-out (negative volume).
        let flipped: Vec<[u32; 3]> = triangles.iter().map(|t| [t[0], t[2], t[1]]).collect();
        assert!(!closed_outward(&positions, &flipped));
        // One face wound the other way: an edge appears twice in the same direction.
        let mut mixed = triangles.clone();
        mixed[0] = [mixed[0][0], mixed[0][2], mixed[0][1]];
        mixed[1] = [mixed[1][0], mixed[1][2], mixed[1][1]];
        assert!(!closed_outward(&positions, &mixed));
        // A single quad (a fin): open.
        assert!(!closed_outward(&positions[..4], &triangles[..2]));
        // Out-of-range index: not closed (and no panic).
        assert!(!closed_outward(&positions, &[[0, 1, 999]]));
    }

    #[test]
    fn a_degenerate_triangle_does_not_open_a_closed_mesh() {
        let (positions, mut triangles) = cube();
        triangles.push([0, 0, 1]);
        assert!(closed_outward(&positions, &triangles));
    }
}
