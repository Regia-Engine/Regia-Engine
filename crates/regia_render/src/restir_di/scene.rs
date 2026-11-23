// SPDX-License-Identifier: MPL-2.0
//! The deterministic synthetic scene the many-light tests and `render.many_light_stats` run on
//! (ADR-2470): an analytic G-buffer (a ground plane, a back wall and a row of boxes, ray cast per
//! pixel) under a field of point and spot lights scattered by a fixed hash.

use regia_core::glam::{Mat4, Vec3, Vec4};

use super::cpu::GBuffer;
use super::{ManyLight, RestirCamera, pcg};

/// A synthetic frame: camera, G-buffer and lights.
#[derive(Clone, Debug)]
pub struct SyntheticScene {
    /// The camera the G-buffer was cast from.
    pub camera: RestirCamera,
    /// The G-buffer.
    pub gbuffer: GBuffer,
    /// The lights.
    pub lights: Vec<ManyLight>,
}

/// `[0, 1)` from the hash of `(seed, i, k)`.
fn hash01(seed: u32, i: u32, k: u32) -> f32 {
    (pcg(seed ^ pcg(i ^ pcg(k.wrapping_add(0x5bd1_e995)))) >> 8) as f32 * (1.0 / 16_777_216.0)
}

/// An axis-aligned box of the scene.
struct SceneBox {
    lo: [f32; 3],
    hi: [f32; 3],
    albedo: [f32; 3],
    roughness: f32,
    metallic: f32,
}

const fn scene_box(lo: [f32; 3], hi: [f32; 3], albedo: [f32; 3], roughness: f32, metallic: f32) -> SceneBox {
    SceneBox {
        lo,
        hi,
        albedo,
        roughness,
        metallic,
    }
}

const BOXES: &[SceneBox] = &[
    scene_box([-9.0, 0.0, 6.0], [-6.5, 3.5, 8.0], [0.7, 0.3, 0.2], 0.6, 0.0),
    scene_box([-3.0, 0.0, 10.0], [-1.0, 5.0, 12.0], [0.9, 0.9, 0.9], 0.3, 1.0),
    scene_box([2.0, 0.0, 4.0], [4.0, 1.5, 6.5], [0.2, 0.5, 0.8], 0.8, 0.0),
    scene_box([6.0, 0.0, 14.0], [9.0, 4.0, 16.0], [0.5, 0.7, 0.3], 0.45, 0.0),
];

/// Back wall plane `z = WALL_Z`.
const WALL_Z: f32 = 30.0;

struct Hit {
    t: f32,
    normal: Vec3,
    albedo: [f32; 3],
    roughness: f32,
    metallic: f32,
}

fn cast(origin: Vec3, dir: Vec3) -> Option<Hit> {
    let mut best: Option<Hit> = None;
    let mut take = |hit: Hit| {
        if hit.t > 1e-3 && best.as_ref().is_none_or(|b| hit.t < b.t) {
            best = Some(hit);
        }
    };
    if dir.y < -1e-6 {
        let t = -origin.y / dir.y;
        let p = origin + dir * t;
        let checker = ((p.x.floor() as i32 + p.z.floor() as i32) & 1) as f32;
        take(Hit {
            t,
            normal: Vec3::Y,
            albedo: [0.35 + 0.3 * checker, 0.35 + 0.25 * checker, 0.3 + 0.2 * checker],
            roughness: 0.9 - 0.5 * checker,
            metallic: 0.0,
        });
    }
    if dir.z > 1e-6 {
        let t = (WALL_Z - origin.z) / dir.z;
        take(Hit {
            t,
            normal: Vec3::NEG_Z,
            albedo: [0.6, 0.6, 0.55],
            roughness: 0.7,
            metallic: 0.0,
        });
    }
    for b in BOXES {
        let (lo, hi) = (Vec3::from(b.lo), Vec3::from(b.hi));
        let inv = dir.recip();
        let t0 = (lo - origin) * inv;
        let t1 = (hi - origin) * inv;
        let tmin = t0.min(t1);
        let tmax = t0.max(t1);
        let near = tmin.max_element();
        let far = tmax.min_element();
        if near <= far && far > 0.0 {
            let axis = if tmin.x >= tmin.y && tmin.x >= tmin.z {
                0
            } else if tmin.y >= tmin.z {
                1
            } else {
                2
            };
            let mut normal = Vec3::ZERO;
            normal[axis] = -dir[axis].signum();
            take(Hit {
                t: near,
                normal,
                albedo: b.albedo,
                roughness: b.roughness,
                metallic: b.metallic,
            });
        }
    }
    best
}

impl SyntheticScene {
    /// The default view of the scene at `width × height` with `light_count` lights from `seed`.
    #[must_use]
    pub fn new(width: u32, height: u32, light_count: u32, seed: u32) -> Self {
        let camera = Self::camera_at(width, height, 0.0);
        Self::with_camera(width, height, light_count, seed, camera, None)
    }

    /// The default camera moved `dx` metres to the right (a camera step for the temporal tests).
    #[must_use]
    pub fn camera_at(width: u32, height: u32, dx: f32) -> RestirCamera {
        RestirCamera::look_at(
            [dx, 6.0, -14.0],
            [dx, 0.5, 8.0],
            1.0,
            width as f32 / height.max(1) as f32,
            0.1,
            200.0,
        )
    }

    /// The scene seen from `camera`. With `previous`, the motion vectors point to where each
    /// surface was in that camera's image (`prev_uv - uv`, the frame's velocity convention).
    #[must_use]
    pub fn with_camera(
        width: u32,
        height: u32,
        light_count: u32,
        seed: u32,
        camera: RestirCamera,
        previous: Option<&RestirCamera>,
    ) -> Self {
        let n = (width * height) as usize;
        let mut g = GBuffer {
            width,
            height,
            depth: vec![0.0; n],
            normal: vec![[0.0, 1.0, 0.0]; n],
            albedo: vec![[0.0; 3]; n],
            roughness: vec![1.0; n],
            metallic: vec![0.0; n],
            motion: vec![[0.0; 2]; n],
        };
        let world_from_clip = camera.clip_from_world.inverse();
        let eye = Vec3::from(camera.eye);
        let forward = Vec3::from(camera.forward);
        for y in 0..height {
            for x in 0..width {
                let i = (y * width + x) as usize;
                let uv = [(x as f32 + 0.5) / width as f32, (y as f32 + 0.5) / height as f32];
                // Unit ray through the pixel: the reconstruction at depth 1, normalized.
                let dir = (super::world_at_uv(world_from_clip, eye, forward, uv, 1.0) - eye).normalize();
                let Some(hit) = cast(eye, dir) else { continue };
                let depth = hit.t * dir.dot(forward);
                if depth <= camera.near || depth >= camera.far {
                    continue;
                }
                g.depth[i] = depth;
                g.normal[i] = hit.normal.to_array();
                g.albedo[i] = hit.albedo;
                g.roughness[i] = hit.roughness;
                g.metallic[i] = hit.metallic;
                if let Some(prev) = previous {
                    let p = eye + dir * hit.t;
                    let c = prev.clip_from_world * Vec4::new(p.x, p.y, p.z, 1.0);
                    if c.w > 1e-4 {
                        let ndc = c.truncate() / c.w;
                        let prev_uv = [ndc.x * 0.5 + 0.5, 0.5 - ndc.y * 0.5];
                        g.motion[i] = [prev_uv[0] - uv[0], prev_uv[1] - uv[1]];
                    }
                }
            }
        }
        Self {
            camera,
            gbuffer: g,
            lights: scatter_lights(light_count, seed),
        }
    }
}

/// `count` lights over the scene from `seed`: 85 % points of 150 to 1,500 lm, 15 % downward spots
/// of 40 to 400 cd, influence radii 2 to 7 m, assorted colors.
#[must_use]
pub fn scatter_lights(count: u32, seed: u32) -> Vec<ManyLight> {
    (0..count)
        .map(|i| {
            let h = |k: u32| hash01(seed, i, k);
            let position = [-20.0 + 40.0 * h(0), 0.3 + 3.7 * h(1), -4.0 + 34.0 * h(2)];
            let hue = h(3);
            let color = [
                0.4 + 0.6 * (hue * std::f32::consts::TAU).cos().abs(),
                0.4 + 0.6 * ((hue + 0.33) * std::f32::consts::TAU).cos().abs(),
                0.4 + 0.6 * ((hue + 0.66) * std::f32::consts::TAU).cos().abs(),
            ];
            let radius = 2.0 + 5.0 * h(4);
            if h(5) < 0.15 {
                let tilt = [0.6 * (h(6) - 0.5), -1.0, 0.6 * (h(7) - 0.5)];
                ManyLight::spot(
                    position,
                    tilt,
                    color,
                    40.0 + 360.0 * h(8),
                    [0.3, 0.6],
                    radius,
                    0.0,
                )
            } else {
                ManyLight::point(position, color, 150.0 + 1350.0 * h(8), radius, 0.0)
            }
        })
        .collect()
}

/// The matrix the GPU passes reconstruct with: kept beside the camera so a test can feed the same.
#[must_use]
pub fn world_from_clip(camera: &RestirCamera) -> Mat4 {
    camera.clip_from_world.inverse()
}
