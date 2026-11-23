// SPDX-License-Identifier: MPL-2.0
//! Geometric oracles for the slot tests: view rays through pixels, the tube's triangles, sun
//! visibility in the cut, and fog along a ray. Plain CPU geometry, independent of the renderer.

use regia_core::glam::Vec3;
use regia_render::FogVolume;

use crate::scene::{self, CamFrame, H, W};

/// One triangle of the tube (world space).
pub type Tri = [Vec3; 3];

/// The tube's triangles (floor, ramp, walls, roof, headwall, end cap, lip): the render meshes.
pub fn tube_triangles() -> Vec<Tri> {
    let mut tris = Vec::new();
    for mesh in scene::tunnel_meshes(0.0) {
        let p = |i: u32| Vec3::from_array(mesh.vertices[i as usize].position);
        for &[a, b, c] in mesh.indices.as_chunks::<3>().0 {
            tris.push([p(a), p(b), p(c)]);
        }
    }
    tris
}

/// Möller–Trumbore, double-sided: distance along `d` (unit) to `tri`, if hit at `t > 1e-4`.
pub fn hit_triangle(o: Vec3, d: Vec3, tri: &Tri) -> Option<f32> {
    let e1 = tri[1] - tri[0];
    let e2 = tri[2] - tri[0];
    let p = d.cross(e2);
    let det = e1.dot(p);
    if det.abs() < 1e-9 {
        return None;
    }
    let inv = 1.0 / det;
    let s = o - tri[0];
    let u = s.dot(p) * inv;
    if !(0.0..=1.0).contains(&u) {
        return None;
    }
    let q = s.cross(e1);
    let v = d.dot(q) * inv;
    if v < 0.0 || u + v > 1.0 {
        return None;
    }
    let t = e2.dot(q) * inv;
    (t > 1e-4).then_some(t)
}

/// Nearest triangle hit along `d` from `o`.
pub fn first_hit(tris: &[Tri], o: Vec3, d: Vec3) -> Option<f32> {
    tris.iter()
        .filter_map(|tri| hit_triangle(o, d, tri))
        .fold(None, |best: Option<f32>, t| Some(best.map_or(t, |b| b.min(t))))
}

/// Whether the sun (`to_sun`, unit, pointing at the sun) reaches `p` past the tube's
/// triangles. Inside the cut and the tube every other blocker (the ground beyond the trench
/// walls) lies behind a wall, so the triangles are the whole test there.
pub fn sun_visible(tris: &[Tri], p: Vec3, to_sun: Vec3) -> bool {
    first_hit(tris, p, to_sun).is_none()
}

/// The unit view ray through the centre of pixel `(x, y)` of `cam` (origin at the eye).
pub fn pixel_ray(cam: &CamFrame, x: u32, y: u32) -> (Vec3, Vec3) {
    let inv = scene::view_proj(cam).inverse();
    let ndc_x = (x as f32 + 0.5) / W as f32 * 2.0 - 1.0;
    let ndc_y = 1.0 - (y as f32 + 0.5) / H as f32 * 2.0;
    let near = inv.project_point3(Vec3::new(ndc_x, ndc_y, 0.0));
    let far = inv.project_point3(Vec3::new(ndc_x, ndc_y, 1.0));
    let eye = Vec3::from_array(cam.eye);
    (eye, (far - near).normalize())
}

/// Where `p` projects to in `cam`'s frame (pixel coordinates), if in front of the camera.
pub fn project(cam: &CamFrame, p: Vec3) -> Option<[f32; 2]> {
    let clip = scene::view_proj(cam) * p.extend(1.0);
    if clip.w <= 1e-4 {
        return None;
    }
    let ndc = clip.truncate() / clip.w;
    Some([(ndc.x + 1.0) * 0.5 * W as f32, (1.0 - ndc.y) * 0.5 * H as f32])
}

/// What lights the fog along a view ray: the tube's triangles, the fog volumes and the sun.
pub struct FogRays<'a> {
    /// Sun blockers.
    pub tris: &'a [Tri],
    /// The fog.
    pub volumes: &'a [FogVolume],
    /// Unit vector toward the sun.
    pub to_sun: Vec3,
}

impl FogRays<'_> {
    /// Fog along the view ray `o + t d` up to `t_max`: `(lit, shadowed)` integrals of density ×
    /// length (optical depths) over the air the sun reaches and the air it does not, each
    /// restricted to the points `keep` accepts. Midpoint rule, 0.25 m steps.
    pub fn along(&self, o: Vec3, d: Vec3, t_max: f32, keep: &dyn Fn(Vec3) -> bool) -> (f32, f32) {
        const STEP: f32 = 0.25;
        let (mut lit, mut shadowed) = (0.0, 0.0);
        let n = (t_max / STEP).ceil() as u32;
        for i in 0..n {
            let t0 = i as f32 * STEP;
            let len = STEP.min(t_max - t0);
            let p = o + d * (t0 + len * 0.5);
            if !keep(p) {
                continue;
            }
            let density: f32 = self.volumes.iter().map(|v| v.density_at(p.to_array())).sum();
            if density <= 0.0 {
                continue;
            }
            if sun_visible(self.tris, p, self.to_sun) {
                lit += density * len;
            } else {
                shadowed += density * len;
            }
        }
        (lit, shadowed)
    }
}

/// Mean of `values`, 0 when empty.
pub fn mean(values: &[f32]) -> f32 {
    if values.is_empty() {
        0.0
    } else {
        values.iter().sum::<f32>() / values.len() as f32
    }
}

/// Display luma (BT.709 of the 8-bit channels, 0–255) of pixel `(x, y)`.
pub fn luma_at(px: &[u8], x: u32, y: u32) -> f32 {
    let i = ((y * W + x) * 4) as usize;
    0.2126 * f32::from(px[i]) + 0.7152 * f32::from(px[i + 1]) + 0.0722 * f32::from(px[i + 2])
}
