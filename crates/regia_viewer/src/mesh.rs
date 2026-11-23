// SPDX-License-Identifier: MPL-2.0
//! CPU mesh the viewer uploads: what a product package's cooked mesh decodes to.

use bytemuck::{Pod, Zeroable};
use glam::{Affine3A, Vec3};

use crate::ViewerError;

/// One vertex. A faceted stone has unshared vertices: one normal and one facet number per facet.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
pub struct ViewerVertex {
    /// Position in millimeters.
    pub position: [f32; 3],
    /// Unit normal.
    pub normal: [f32; 3],
    /// Texture coordinate.
    pub uv: [f32; 2],
    /// The facet number (0 on smooth parts), the same on every vertex of a facet.
    pub facet: f32,
}

/// Size of [`ViewerVertex`] in bytes.
pub const VERTEX_STRIDE: u64 = std::mem::size_of::<ViewerVertex>() as u64;

/// An axis-aligned box in millimeters.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewerBounds {
    /// The smallest corner.
    pub min: [f32; 3],
    /// The largest corner.
    pub max: [f32; 3],
}

impl ViewerBounds {
    /// The box of a mesh placed by `transform`.
    #[must_use]
    pub fn transformed(&self, transform: &Affine3A) -> Self {
        let min = Vec3::from(self.min);
        let max = Vec3::from(self.max);
        let mut out_min = Vec3::splat(f32::INFINITY);
        let mut out_max = Vec3::splat(f32::NEG_INFINITY);
        for i in 0..8u32 {
            let corner = Vec3::new(
                if i & 1 == 0 { min.x } else { max.x },
                if i & 2 == 0 { min.y } else { max.y },
                if i & 4 == 0 { min.z } else { max.z },
            );
            let p = transform.transform_point3(corner);
            out_min = out_min.min(p);
            out_max = out_max.max(p);
        }
        Self {
            min: out_min.to_array(),
            max: out_max.to_array(),
        }
    }

    /// The box around both.
    #[must_use]
    pub fn union(&self, other: &Self) -> Self {
        Self {
            min: Vec3::from(self.min).min(Vec3::from(other.min)).to_array(),
            max: Vec3::from(self.max).max(Vec3::from(other.max)).to_array(),
        }
    }
}

/// A mesh ready to upload.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ViewerMeshData {
    /// The vertices.
    pub vertices: Vec<ViewerVertex>,
    /// Triangle list indices, counter-clockwise seen from the outside.
    pub indices: Vec<u32>,
}

impl ViewerMeshData {
    /// Refuses what would draw garbage: no triangles, a partial triangle, an index out of range or a
    /// position or normal that is not finite. Package data goes through here before it reaches the GPU.
    ///
    /// # Errors
    /// [`ViewerError::BadMesh`] with the reason.
    pub fn validate(&self) -> Result<(), ViewerError> {
        if self.indices.is_empty() || self.vertices.is_empty() {
            return Err(ViewerError::BadMesh("the mesh has no triangles".to_string()));
        }
        if !self.indices.len().is_multiple_of(3) {
            return Err(ViewerError::BadMesh(format!(
                "{} indices do not form whole triangles",
                self.indices.len()
            )));
        }
        let count = self.vertices.len();
        if let Some(bad) = self.indices.iter().find(|i| **i as usize >= count) {
            return Err(ViewerError::BadMesh(format!(
                "index {bad} is out of range of {count} vertices"
            )));
        }
        let finite = self.vertices.iter().all(|v| {
            v.position
                .iter()
                .chain(v.normal.iter())
                .chain(v.uv.iter())
                .all(|c| c.is_finite())
                && v.facet.is_finite()
        });
        if !finite {
            return Err(ViewerError::BadMesh(
                "a vertex holds a value that is not finite".to_string(),
            ));
        }
        Ok(())
    }

    /// The box around every vertex. All zero for an empty mesh.
    #[must_use]
    pub fn bounds(&self) -> ViewerBounds {
        let Some(first) = self.vertices.first() else {
            return ViewerBounds {
                min: [0.0; 3],
                max: [0.0; 3],
            };
        };
        let mut min = Vec3::from(first.position);
        let mut max = min;
        for v in &self.vertices {
            let p = Vec3::from(v.position);
            min = min.min(p);
            max = max.max(p);
        }
        ViewerBounds {
            min: min.to_array(),
            max: max.to_array(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vertex(x: f32) -> ViewerVertex {
        ViewerVertex {
            position: [x, 0.0, 0.0],
            normal: [0.0, 0.0, 1.0],
            uv: [0.0; 2],
            facet: 0.0,
        }
    }

    #[test]
    fn bad_meshes_are_refused_with_a_reason() {
        let empty = ViewerMeshData::default();
        assert!(matches!(empty.validate(), Err(ViewerError::BadMesh(_))));

        let partial = ViewerMeshData {
            vertices: vec![vertex(0.0); 3],
            indices: vec![0, 1],
        };
        assert!(matches!(partial.validate(), Err(ViewerError::BadMesh(_))));

        let range = ViewerMeshData {
            vertices: vec![vertex(0.0); 3],
            indices: vec![0, 1, 3],
        };
        assert!(matches!(range.validate(), Err(ViewerError::BadMesh(_))));

        let nan = ViewerMeshData {
            vertices: vec![vertex(f32::NAN); 3],
            indices: vec![0, 1, 2],
        };
        assert!(matches!(nan.validate(), Err(ViewerError::BadMesh(_))));

        let good = ViewerMeshData {
            vertices: vec![vertex(0.0); 3],
            indices: vec![0, 1, 2],
        };
        assert_eq!(good.validate(), Ok(()));
    }

    #[test]
    fn bounds_follow_a_transform() {
        let mesh = ViewerMeshData {
            vertices: vec![vertex(-1.0), vertex(3.0)],
            indices: vec![],
        };
        let bounds = mesh.bounds();
        assert_eq!(bounds.min[0], -1.0);
        assert_eq!(bounds.max[0], 3.0);
        let moved = bounds.transformed(&Affine3A::from_translation(Vec3::new(10.0, 0.0, 0.0)));
        assert_eq!(moved.min[0], 9.0);
        assert_eq!(moved.max[0], 13.0);
        let mirrored = bounds.transformed(&Affine3A::from_scale(Vec3::new(-1.0, 1.0, 1.0)));
        assert_eq!(mirrored.min[0], -3.0);
        assert_eq!(mirrored.max[0], 1.0);
    }
}
