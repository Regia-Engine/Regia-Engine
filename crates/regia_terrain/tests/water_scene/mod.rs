// SPDX-License-Identifier: MPL-2.0
//! A small stream scene for the water tests and `examples/water_shots.rs`: a carved channel with
//! a pebble bed, three boulders that break the surface, grassy banks, and a sloped ribbon of
//! water down the channel (ADR-1399).

#![allow(dead_code)] // shared by a test and an example, each uses part of it

use regia_core::glam::{Mat4, Vec3};
use regia_render::{
    DirectionalLightUniform, FogSettings, FrameMesh, FrameQuality, FrameScene, LabMaterial, LabVertex,
    PostSettings, ShadowCast,
};
use regia_terrain::{RibbonPoint, WaterBody, WaterLook, WaterObstacle};

/// Water surface fall along x (m per m).
pub const FALL: f32 = 0.01;
/// Water depth on the centerline (m).
pub const DEPTH: f32 = 0.35;
/// Authored water width (m): where the banks reach the surface.
pub const WIDTH: f32 = 3.6;

/// Water surface height at `x`.
pub fn surface(x: f32) -> f32 {
    -FALL * x
}

fn smoothstep(a: f32, b: f32, x: f32) -> f32 {
    let t = ((x - a) / (b - a)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Ground height: the bed `DEPTH` under the surface, banks rising to 0.45 m over it.
pub fn ground(x: f32, z: f32) -> f32 {
    let bank = smoothstep(1.0, 2.6, z.abs());
    surface(x) - DEPTH + (DEPTH + 0.45) * bank + 0.04 * (0.7 * x).sin() * bank
}

/// One mesh instance of the scene.
pub struct Object {
    pub vertices: Vec<LabVertex>,
    pub indices: Vec<u32>,
    pub material: LabMaterial,
    pub mesh_key: u64,
    pub material_key: u64,
    pub model: [[f32; 4]; 4],
}

/// The stream scene.
pub struct StreamScene {
    pub objects: Vec<Object>,
    pub stream: WaterBody,
    /// World positions of pebbles: (center, light?).
    pub pebbles: Vec<([f32; 3], bool)>,
}

struct Lcg(u64);

impl Lcg {
    fn unit(&mut self) -> f32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((self.0 >> 40) as f32) / ((1u64 << 24) as f32)
    }
}

fn grid(x0: f32, x1: f32, z0: f32, z1: f32, nx: u32, nz: u32) -> (Vec<LabVertex>, Vec<u32>) {
    let mut vertices = Vec::new();
    for j in 0..=nz {
        for i in 0..=nx {
            let x = x0 + (x1 - x0) * i as f32 / nx as f32;
            let z = z0 + (z1 - z0) * j as f32 / nz as f32;
            let e = 0.02;
            let dx = (ground(x + e, z) - ground(x - e, z)) / (2.0 * e);
            let dz = (ground(x, z + e) - ground(x, z - e)) / (2.0 * e);
            let n = Vec3::new(-dx, 1.0, -dz).normalize();
            vertices.push(LabVertex::from_pos_normal_uv(
                [x, ground(x, z), z],
                n.to_array(),
                [x, z],
            ));
        }
    }
    let mut indices = Vec::new();
    let row = nx + 1;
    for j in 0..nz {
        for i in 0..nx {
            let a = j * row + i;
            indices.extend_from_slice(&[a, a + row, a + 1, a + 1, a + row, a + row + 1]);
        }
    }
    (vertices, indices)
}

/// A unit octahedron subdivided once, flat shaded (a rough stone).
fn stone() -> (Vec<LabVertex>, Vec<u32>) {
    let p = [Vec3::X, Vec3::NEG_X, Vec3::Y, Vec3::NEG_Y, Vec3::Z, Vec3::NEG_Z];
    let faces = [
        [0, 2, 4],
        [4, 2, 1],
        [1, 2, 5],
        [5, 2, 0],
        [4, 3, 0],
        [1, 3, 4],
        [5, 3, 1],
        [0, 3, 5],
    ];
    let mut tris = Vec::new();
    for f in faces {
        let (a, b, c) = (p[f[0]], p[f[1]], p[f[2]]);
        let (ab, bc, ca) = (
            (a + b).normalize() * 0.93,
            (b + c).normalize() * 1.05,
            (c + a).normalize() * 0.97,
        );
        tris.extend_from_slice(&[[a, ab, ca], [ab, b, bc], [ca, bc, c], [ab, bc, ca]]);
    }
    let mut vertices = Vec::new();
    for [a, b, c] in tris {
        let n = (b - a).cross(c - a).normalize();
        for v in [a, b, c] {
            vertices.push(LabVertex::from_pos_normal_uv(
                v.to_array(),
                n.to_array(),
                [0.0, 0.0],
            ));
        }
    }
    let indices = (0..vertices.len() as u32).collect();
    (vertices, indices)
}

fn material(rgb: [f32; 3], roughness: f32) -> LabMaterial {
    LabMaterial {
        base_color: [rgb[0], rgb[1], rgb[2], 1.0],
        roughness,
        ..LabMaterial::EMPTY
    }
}

/// Builds the scene.
pub fn stream_scene() -> StreamScene {
    let mut objects = Vec::new();
    let (bed_v, bed_i) = grid(-10.0, 70.0, -2.4, 2.4, 160, 24);
    objects.push(Object {
        vertices: bed_v,
        indices: bed_i,
        material: material([0.36, 0.31, 0.25], 0.9),
        mesh_key: 1,
        material_key: 1,
        model: Mat4::IDENTITY.to_cols_array_2d(),
    });
    for (key, z0, z1) in [(2u64, 2.4f32, 14.0f32), (3, -14.0, -2.4)] {
        let (v, i) = grid(-10.0, 70.0, z0, z1, 80, 24);
        objects.push(Object {
            vertices: v,
            indices: i,
            material: material([0.2, 0.26, 0.12], 0.95),
            mesh_key: key,
            material_key: 2,
            model: Mat4::IDENTITY.to_cols_array_2d(),
        });
    }
    let (stone_v, stone_i) = stone();
    let mut rng = Lcg(0x57AE_A3B1_2026_1022);
    let mut pebbles = Vec::new();
    for _ in 0..90 {
        let x = rng.unit() * 34.0 - 2.0;
        let z = (rng.unit() * 2.0 - 1.0) * 1.5;
        let r = 0.07 + 0.11 * rng.unit();
        let light = rng.unit() > 0.5;
        let y = ground(x, z) + 0.25 * r;
        pebbles.push(([x, y, z], light));
        let model = Mat4::from_translation(Vec3::new(x, y, z))
            * Mat4::from_rotation_y(rng.unit() * std::f32::consts::TAU)
            * Mat4::from_scale(Vec3::new(r * 1.2, r * 0.55, r));
        objects.push(Object {
            vertices: stone_v.clone(),
            indices: stone_i.clone(),
            material: if light {
                material([0.72, 0.68, 0.6], 0.6)
            } else {
                material([0.16, 0.15, 0.14], 0.6)
            },
            mesh_key: 10,
            material_key: if light { 10 } else { 11 },
            model: model.to_cols_array_2d(),
        });
    }
    let boulders = [(6.0f32, 0.3f32, 0.38f32), (14.0, -0.6, 0.3), (22.0, 0.5, 0.42)];
    for (x, z, r) in boulders {
        let model = Mat4::from_translation(Vec3::new(x, ground(x, z) + 0.55 * r, z))
            * Mat4::from_scale(Vec3::new(r, r * 1.25, r * 0.9));
        objects.push(Object {
            vertices: stone_v.clone(),
            indices: stone_i.clone(),
            material: material([0.42, 0.4, 0.37], 0.75),
            mesh_key: 10,
            material_key: 12,
            model: model.to_cols_array_2d(),
        });
    }
    let points = (0..27)
        .map(|i| {
            let x = -8.0 + 2.0 * i as f32;
            RibbonPoint {
                position: [x, surface(x), 0.0],
                width: WIDTH,
                depth: DEPTH,
                speed: None,
            }
        })
        .collect();
    let mut stream = WaterBody::ribbon(points, WaterLook::clear_stream());
    stream.obstacles = boulders
        .iter()
        .map(|&(x, z, r)| WaterObstacle {
            position: [x, 0.0, z],
            radius: r * 0.8,
            strength: 1.0,
        })
        .collect();
    StreamScene {
        objects,
        stream,
        pebbles,
    }
}

/// The studio sky the scene is lit and seen against (cd/m²: sky above, ground below), for
/// `FrameRenderer::set_studio_sky`.
pub const STUDIO_SKY: ([f32; 3], [f32; 3]) = ([4200.0, 5200.0, 6900.0], [900.0, 850.0, 700.0]);

/// A camera.
#[derive(Clone, Copy, Debug)]
pub struct Camera {
    pub eye: [f32; 3],
    pub target: [f32; 3],
    pub fov_y: f32,
}

/// About 2 m from the water, looking down the stream.
pub const NEAR: Camera = Camera {
    eye: [-1.2, 1.25, 1.9],
    target: [5.0, -0.25, 0.0],
    fov_y: 0.9,
};

/// About 2 m above the water, looking down at the bed.
pub const TOP: Camera = Camera {
    eye: [3.6, 2.0, 0.9],
    target: [4.6, -0.35, -0.1],
    fov_y: 0.9,
};

/// About 200 m away, high on a hillside.
pub const FAR: Camera = Camera {
    eye: [-150.0, 110.0, 90.0],
    target: [12.0, 0.0, 0.0],
    fov_y: 0.25,
};

/// Post settings like Desktop High at EV 15 (HDR frame, exposure, AgX).
pub fn post() -> PostSettings {
    PostSettings {
        enabled: true,
        bloom: false,
        auto_exposure: false,
        exposure_ev: 14.0,
        fxaa: false,
        taa: false,
        ..PostSettings::default()
    }
}

/// The frame description of `scene` seen from `camera`.
pub fn frame<'a>(
    scene: &'a StreamScene,
    camera: Camera,
    width: u32,
    height: u32,
    post: &'a PostSettings,
    sun: [f32; 3],
) -> FrameScene<'a> {
    FrameScene {
        width,
        height,
        eye: camera.eye,
        target: camera.target,
        fov_y: camera.fov_y,
        near: 0.1,
        far: 2000.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 14.0,
        ambient_lux: 18_000.0,
        double_sided: false,
        post: Some(post),
        fog: FogSettings::default(),
        directional: DirectionalLightUniform::new(sun, [1.0, 0.96, 0.9], 90_000.0),
        local_lights: &[],
        quality: FrameQuality::LAB,
        meshes: scene
            .objects
            .iter()
            .enumerate()
            .map(|(i, o)| FrameMesh {
                is_static: false,
                mesh_key: o.mesh_key,
                material_key: o.material_key,
                object_key: i as u64 + 1,
                vertices: &o.vertices,
                indices: &o.indices,
                material: &o.material,
                model: o.model,
                joints: &[],
                receiver_layer: 0,
                shadow: ShadowCast::All,
                uv1: &[],
                lod_dither: 0.0,
            })
            .collect(),
    }
}

/// The pixel `world` projects to, if on screen.
pub fn project(camera: Camera, width: u32, height: u32, world: [f32; 3]) -> Option<(u32, u32)> {
    let view = regia_render::view_math::look_at_view(
        Vec3::from_array(camera.eye),
        Vec3::from_array(camera.target),
        Vec3::Y,
    );
    let proj = regia_core::glam::camera::rh::proj::directx::perspective(
        camera.fov_y,
        width as f32 / height as f32,
        0.1,
        2000.0,
    );
    let clip = proj * view * Vec3::from_array(world).extend(1.0);
    if clip.w <= 0.0 {
        return None;
    }
    let x = (clip.x / clip.w * 0.5 + 0.5) * width as f32;
    let y = (0.5 - clip.y / clip.w * 0.5) * height as f32;
    (x >= 0.0 && y >= 0.0 && x < width as f32 && y < height as f32).then_some((x as u32, y as u32))
}
