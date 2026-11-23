// SPDX-License-Identifier: MPL-2.0
//! Signed-distance primitives, mesh volumes and sphere tracing (M15b).
//!
//! Probe rays march these distances. Hardware ray queries are not required.
//!
//! ## Provenance
//!
//! Sphere tracing: Hart, "Sphere Tracing: A Geometric Method for the Antialiased
//! Ray Tracing of Implicit Surfaces", The Visual Computer 1996.
//! Box and sphere distances: Quilez, "Distance Functions" (iquilezles.org, public domain).
//! Closest point on a triangle: Ericson, *Real-Time Collision Detection*, 2005, §5.1.5.
//! Mesh sign: the closest triangle's geometric normal (a one-triangle stand-in for
//! the angle-weighted pseudonormal of Bærentzen and Aanæs, "Signed Distance
//! Computation Using the Angle Weighted Pseudonormal", IEEE TVCG 2005).
//! Heightfield: the graph distance `(y − h) / sqrt(1 + |∇h|²)`.

use regia_core::glam::Vec3;

/// Vertical distance to a heightfield, shortened by the slope so a march does not tunnel.
///
/// `slope` is `|∇h|` (0 on flat ground). Positive is above the surface.
#[must_use]
pub fn heightfield_distance(y: f32, surface_height: f32, slope: f32) -> f32 {
    let scale = (1.0 + slope * slope).sqrt().max(1.0e-4);
    (y - surface_height) / scale
}

/// A bilinear height grid in metres. `samples` is row-major, row 0 at `origin_z`.
#[derive(Clone, Debug, PartialEq)]
pub struct HeightfieldGrid {
    /// World X of column 0.
    pub origin_x: f32,
    /// World Z of row 0.
    pub origin_z: f32,
    /// Metres between columns.
    pub spacing_x: f32,
    /// Metres between rows.
    pub spacing_z: f32,
    /// Column count (at least 2).
    pub width: u32,
    /// Row count (at least 2).
    pub height: u32,
    /// Heights in metres, length `width * height`.
    pub samples: Vec<f32>,
    /// Albedo used when a ray hits the field.
    pub albedo: [f32; 3],
}

impl HeightfieldGrid {
    /// Bilinear height. Outside the grid the border sample is repeated.
    #[must_use]
    pub fn sample_height(&self, x: f32, z: f32) -> f32 {
        if self.width < 2 || self.height < 2 || self.spacing_x <= 0.0 || self.spacing_z <= 0.0 {
            return 0.0;
        }
        let u = ((x - self.origin_x) / self.spacing_x).clamp(0.0, (self.width - 1) as f32);
        let v = ((z - self.origin_z) / self.spacing_z).clamp(0.0, (self.height - 1) as f32);
        let iu = u.floor() as u32;
        let iv = v.floor() as u32;
        let iu1 = (iu + 1).min(self.width - 1);
        let iv1 = (iv + 1).min(self.height - 1);
        let fu = u - iu as f32;
        let fv = v - iv as f32;
        let h = |col: u32, row: u32| -> f32 {
            self.samples
                .get((row * self.width + col) as usize)
                .copied()
                .unwrap_or(0.0)
        };
        let a = h(iu, iv) + (h(iu1, iv) - h(iu, iv)) * fu;
        let b = h(iu, iv1) + (h(iu1, iv1) - h(iu, iv1)) * fu;
        a + (b - a) * fv
    }

    /// `|∇h|` from central differences of [`Self::sample_height`].
    #[must_use]
    pub fn slope(&self, x: f32, z: f32) -> f32 {
        let sx = self.spacing_x.max(1.0e-3);
        let sz = self.spacing_z.max(1.0e-3);
        let dhdx = (self.sample_height(x + sx, z) - self.sample_height(x - sx, z)) / (2.0 * sx);
        let dhdz = (self.sample_height(x, z + sz) - self.sample_height(x, z - sz)) / (2.0 * sz);
        dhdx.hypot(dhdz)
    }

    /// Signed distance at a world point.
    #[must_use]
    pub fn signed_distance(&self, p: [f32; 3]) -> f32 {
        let h = self.sample_height(p[0], p[2]);
        heightfield_distance(p[1], h, self.slope(p[0], p[2]))
    }
}

/// Distance, albedo and emission at a point. The smallest distance wins a union.
#[derive(Clone, Copy, Debug)]
pub struct SdfSample {
    /// Signed distance, metres. Negative is inside.
    pub distance: f32,
    /// Linear albedo of the closest surface.
    pub albedo: [f32; 3],
    /// Linear emissive radiance added on a hit.
    pub emissive: [f32; 3],
}

impl SdfSample {
    fn miss() -> Self {
        Self {
            distance: 1.0e4,
            albedo: [0.0; 3],
            emissive: [0.0; 3],
        }
    }
}

/// One primitive in an [`SdfScene`].
#[derive(Clone, Debug, PartialEq)]
pub enum SdfShape {
    /// Round primitive.
    Sphere {
        /// Centre.
        center: [f32; 3],
        /// Radius.
        radius: f32,
        /// Linear albedo.
        albedo: [f32; 3],
        /// Emissive radiance.
        emissive: [f32; 3],
    },
    /// Axis-aligned box.
    Box {
        /// Centre.
        center: [f32; 3],
        /// Half extents.
        half: [f32; 3],
        /// Linear albedo.
        albedo: [f32; 3],
        /// Emissive radiance.
        emissive: [f32; 3],
    },
    /// Terrain-style heightfield.
    Heightfield(HeightfieldGrid),
    /// Voxelised triangle mesh.
    Mesh(MeshSdf),
}

/// A union of SDFs.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SdfScene {
    /// Shapes in the union.
    pub shapes: Vec<SdfShape>,
}

impl SdfScene {
    /// Closest shape at `p`.
    #[must_use]
    pub fn sample(&self, p: [f32; 3]) -> SdfSample {
        let mut best = SdfSample::miss();
        for shape in &self.shapes {
            let sample = match shape {
                SdfShape::Sphere {
                    center,
                    radius,
                    albedo,
                    emissive,
                } => {
                    let d = (Vec3::from_array(p) - Vec3::from_array(*center)).length() - *radius;
                    SdfSample {
                        distance: d,
                        albedo: *albedo,
                        emissive: *emissive,
                    }
                }
                SdfShape::Box {
                    center,
                    half,
                    albedo,
                    emissive,
                } => SdfSample {
                    distance: sd_box(
                        Vec3::from_array(p) - Vec3::from_array(*center),
                        Vec3::from_array(*half),
                    ),
                    albedo: *albedo,
                    emissive: *emissive,
                },
                SdfShape::Heightfield(grid) => SdfSample {
                    distance: grid.signed_distance(p),
                    albedo: grid.albedo,
                    emissive: [0.0; 3],
                },
                SdfShape::Mesh(mesh) => SdfSample {
                    distance: mesh.sample(p),
                    albedo: mesh.albedo,
                    emissive: mesh.emissive,
                },
            };
            if sample.distance < best.distance {
                best = sample;
            }
        }
        best
    }

    /// Central-difference normal. Zero when the scene is empty.
    #[must_use]
    pub fn normal(&self, p: [f32; 3]) -> [f32; 3] {
        let e = 1.0e-3;
        let dx = self.sample([p[0] + e, p[1], p[2]]).distance - self.sample([p[0] - e, p[1], p[2]]).distance;
        let dy = self.sample([p[0], p[1] + e, p[2]]).distance - self.sample([p[0], p[1] - e, p[2]]).distance;
        let dz = self.sample([p[0], p[1], p[2] + e]).distance - self.sample([p[0], p[1], p[2] - e]).distance;
        let n = Vec3::new(dx, dy, dz);
        if n.length_squared() < 1.0e-12 {
            [0.0, 1.0, 0.0]
        } else {
            n.normalize().to_array()
        }
    }
}

fn sd_box(p: Vec3, half: Vec3) -> f32 {
    let q = p.abs() - half;
    q.max(Vec3::ZERO).length() + q.max_element().min(0.0)
}

/// A ray hit against an [`SdfScene`].
#[derive(Clone, Copy, Debug)]
pub struct SdfHit {
    /// Distance along the ray.
    pub t: f32,
    /// World position.
    pub position: [f32; 3],
    /// Outward normal.
    pub normal: [f32; 3],
    /// Surface albedo.
    pub albedo: [f32; 3],
    /// Surface emission.
    pub emissive: [f32; 3],
}

/// Sphere-traces `dir` (need not be unit; it is normalised) from `origin`.
#[must_use]
pub fn sphere_trace(scene: &SdfScene, origin: [f32; 3], dir: [f32; 3], max_distance: f32) -> Option<SdfHit> {
    let direction = Vec3::from_array(dir);
    if direction.length_squared() < 1.0e-12 {
        return None;
    }
    let direction = direction.normalize();
    let mut t = 0.0;
    let max_distance = max_distance.max(0.0);
    for _ in 0..64 {
        let p = (Vec3::from_array(origin) + direction * t).to_array();
        let sample = scene.sample(p);
        if sample.distance < 2.0e-3 {
            return Some(SdfHit {
                t,
                position: p,
                normal: scene.normal(p),
                albedo: sample.albedo,
                emissive: sample.emissive,
            });
        }
        t += sample.distance.max(1.0e-3);
        if t > max_distance {
            return None;
        }
    }
    None
}

/// Voxel signed-distance of a triangle mesh.
#[derive(Clone, Debug, PartialEq)]
pub struct MeshSdf {
    /// Minimum corner of voxel `(0,0,0)`.
    pub origin: [f32; 3],
    /// Voxel edge length, metres.
    pub voxel: f32,
    /// Voxels along each axis.
    pub resolution: u32,
    /// Signed distances, x-major then y then z.
    pub values: Vec<f32>,
    /// Albedo of a hit.
    pub albedo: [f32; 3],
    /// Emissive radiance of a hit.
    pub emissive: [f32; 3],
}

impl MeshSdf {
    /// Voxelises `indices` (triangles) at `resolution` (clamped to 4..=24).
    ///
    /// Triangle winding must point outward. The sign is the closest triangle's
    /// geometric normal (a simplified Bærentzen–Aanæs test): positive outside.
    #[must_use]
    pub fn from_triangles(vertices: &[[f32; 3]], indices: &[u32], resolution: u32, albedo: [f32; 3]) -> Self {
        let resolution = resolution.clamp(4, 24);
        let mut min = Vec3::splat(f32::MAX);
        let mut max = Vec3::splat(f32::MIN);
        for v in vertices {
            let p = Vec3::from_array(*v);
            min = min.min(p);
            max = max.max(p);
        }
        if vertices.is_empty() {
            min = Vec3::ZERO;
            max = Vec3::ONE;
        }
        let extent = (max - min).max(Vec3::splat(1.0e-3));
        let voxel = extent.max_element() / resolution as f32;
        let origin = min - Vec3::splat(voxel);
        let res = resolution + 2;
        let mut values = vec![voxel * res as f32; (res * res * res) as usize];
        let mut tris = Vec::new();
        for tri in indices.as_chunks::<3>().0 {
            let Some(a) = vertices.get(tri[0] as usize) else {
                continue;
            };
            let Some(b) = vertices.get(tri[1] as usize) else {
                continue;
            };
            let Some(c) = vertices.get(tri[2] as usize) else {
                continue;
            };
            let pa = Vec3::from_array(*a);
            let pb = Vec3::from_array(*b);
            let pc = Vec3::from_array(*c);
            let n = (pb - pa).cross(pc - pa);
            if n.length_squared() < 1.0e-12 {
                continue;
            }
            tris.push((pa, pb, pc, n.normalize()));
        }
        for z in 0..res {
            for y in 0..res {
                for x in 0..res {
                    let p = origin + Vec3::new(x as f32 + 0.5, y as f32 + 0.5, z as f32 + 0.5) * voxel;
                    let mut best = f32::MAX;
                    let mut signed = voxel * res as f32;
                    for (a, b, c, n) in &tris {
                        let closest = closest_on_triangle(p, *a, *b, *c);
                        let delta = p - closest;
                        let d = delta.length();
                        if d < best {
                            best = d;
                            let sign = if delta.dot(*n) >= 0.0 { 1.0 } else { -1.0 };
                            signed = sign * d;
                        }
                    }
                    values[((z * res + y) * res + x) as usize] = signed;
                }
            }
        }
        Self {
            origin: origin.to_array(),
            voxel,
            resolution: res,
            values,
            albedo,
            emissive: [0.0; 3],
        }
    }

    /// Trilinear sample. Outside the volume the border voxel is repeated.
    #[must_use]
    pub fn sample(&self, p: [f32; 3]) -> f32 {
        if self.resolution < 2 || self.voxel <= 0.0 {
            return 1.0e4;
        }
        let local = (Vec3::from_array(p) - Vec3::from_array(self.origin)) / self.voxel - Vec3::splat(0.5);
        let max_i = (self.resolution - 1) as f32;
        let clamped = local.clamp(Vec3::ZERO, Vec3::splat(max_i));
        let i = clamped.floor();
        let f = clamped - i;
        let ix = i.x as u32;
        let iy = i.y as u32;
        let iz = i.z as u32;
        let res = self.resolution;
        let at = |x: u32, y: u32, z: u32| -> f32 {
            let x = x.min(res - 1);
            let y = y.min(res - 1);
            let z = z.min(res - 1);
            self.values[((z * res + y) * res + x) as usize]
        };
        let lerp = |a: f32, b: f32, t: f32| a + (b - a) * t;
        let c00 = lerp(at(ix, iy, iz), at(ix + 1, iy, iz), f.x);
        let c10 = lerp(at(ix, iy + 1, iz), at(ix + 1, iy + 1, iz), f.x);
        let c01 = lerp(at(ix, iy, iz + 1), at(ix + 1, iy, iz + 1), f.x);
        let c11 = lerp(at(ix, iy + 1, iz + 1), at(ix + 1, iy + 1, iz + 1), f.x);
        lerp(lerp(c00, c10, f.y), lerp(c01, c11, f.y), f.z)
    }
}

/// Closest point on triangle `abc` to `p` (Ericson §5.1.5).
#[must_use]
pub fn closest_on_triangle(p: Vec3, a: Vec3, b: Vec3, c: Vec3) -> Vec3 {
    let ab = b - a;
    let ac = c - a;
    let ap = p - a;
    let d1 = ab.dot(ap);
    let d2 = ac.dot(ap);
    if d1 <= 0.0 && d2 <= 0.0 {
        return a;
    }
    let bp = p - b;
    let d3 = ab.dot(bp);
    let d4 = ac.dot(bp);
    if d3 >= 0.0 && d4 <= d3 {
        return b;
    }
    let vc = d1 * d4 - d3 * d2;
    if vc <= 0.0 && d1 >= 0.0 && d3 <= 0.0 {
        let v = d1 / (d1 - d3);
        return a + ab * v;
    }
    let cp = p - c;
    let d5 = ab.dot(cp);
    let d6 = ac.dot(cp);
    if d6 >= 0.0 && d5 <= d6 {
        return c;
    }
    let vb = d5 * d2 - d1 * d6;
    if vb <= 0.0 && d2 >= 0.0 && d6 <= 0.0 {
        let w = d2 / (d2 - d6);
        return a + ac * w;
    }
    let va = d3 * d6 - d5 * d4;
    if va <= 0.0 && (d4 - d3) >= 0.0 && (d5 - d6) >= 0.0 {
        let w = (d4 - d3) / ((d4 - d3) + (d5 - d6));
        return b + (c - b) * w;
    }
    let denom = 1.0 / (va + vb + vc);
    let v = vb * denom;
    let w = vc * denom;
    a + ab * v + ac * w
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_heightfield_distance_is_vertical_and_a_slope_is_shorter() {
        assert!((heightfield_distance(3.0, 1.0, 0.0) - 2.0).abs() < 1.0e-5);
        let sloped = heightfield_distance(3.0, 1.0, 1.0);
        assert!((sloped - 2.0 / 2.0_f32.sqrt()).abs() < 1.0e-4);
        assert!(sloped < 2.0);
    }

    #[test]
    fn sphere_trace_hits_a_box_and_a_heightfield() {
        let mut scene = SdfScene::default();
        scene.shapes.push(SdfShape::Box {
            center: [0.0, 0.0, 0.0],
            half: [0.5, 0.5, 0.5],
            albedo: [1.0, 0.0, 0.0],
            emissive: [0.0; 3],
        });
        let hit = sphere_trace(&scene, [0.0, 0.0, 3.0], [0.0, 0.0, -1.0], 10.0).expect("box hit");
        assert!((hit.position[2] - 0.5).abs() < 0.02, "hit z {}", hit.position[2]);
        assert!(hit.albedo[0] > 0.5);

        let heights = vec![0.0; 16];
        let grid = HeightfieldGrid {
            origin_x: -2.0,
            origin_z: -2.0,
            spacing_x: 4.0 / 3.0,
            spacing_z: 4.0 / 3.0,
            width: 4,
            height: 4,
            samples: heights,
            albedo: [0.2, 0.8, 0.2],
        };
        assert!(grid.signed_distance([0.0, 1.0, 0.0]) > 0.5);
        assert!(grid.signed_distance([0.0, -1.0, 0.0]) < -0.5);
        let mut ground = SdfScene::default();
        ground.shapes.push(SdfShape::Heightfield(grid));
        let ground_hit = sphere_trace(&ground, [0.0, 2.0, 0.0], [0.0, -1.0, 0.0], 8.0).expect("ground");
        assert!(
            ground_hit.position[1].abs() < 0.05,
            "y {}",
            ground_hit.position[1]
        );
    }

    #[test]
    fn mesh_sdf_of_a_cube_matches_the_analytic_sign() {
        let vertices = [
            [-1.0, -1.0, -1.0],
            [1.0, -1.0, -1.0],
            [1.0, 1.0, -1.0],
            [-1.0, 1.0, -1.0],
            [-1.0, -1.0, 1.0],
            [1.0, -1.0, 1.0],
            [1.0, 1.0, 1.0],
            [-1.0, 1.0, 1.0],
        ];
        // Outward winding: the geometric normal points out of the cube.
        let indices = [
            0, 2, 1, 0, 3, 2, 4, 5, 6, 4, 6, 7, 0, 5, 4, 0, 1, 5, 3, 6, 2, 3, 7, 6, 0, 7, 3, 0, 4, 7, 1, 6,
            5, 1, 2, 6,
        ];
        let mesh = MeshSdf::from_triangles(&vertices, &indices, 8, [0.5, 0.5, 0.5]);
        assert!(mesh.sample([0.0, 0.0, 0.0]) < 0.0, "center is inside");
        assert!(mesh.sample([3.0, 0.0, 0.0]) > 0.0, "outside is positive");
        let p = closest_on_triangle(
            Vec3::new(0.0, 0.0, 2.0),
            Vec3::new(-1.0, -1.0, 0.0),
            Vec3::new(1.0, -1.0, 0.0),
            Vec3::new(0.0, 1.0, 0.0),
        );
        assert!(
            p.z.abs() < 1.0e-4,
            "closest point stays on the triangle, z {}",
            p.z
        );
    }
}
