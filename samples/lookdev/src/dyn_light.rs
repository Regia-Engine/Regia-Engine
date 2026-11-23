// SPDX-License-Identifier: MIT-0
//! `dyn_light`: are dynamic and skinned meshes lit like the terrain they stand on?
//!
//! A look-dev test plate on DO27's maps (Bore Twelve, the Switchbacks): a skinned mannequin in
//! 0.18 grey, the same mannequin in charcoal (0.05) and a rigid vehicle hull (0.18), each 1, 3
//! and 5 m in front of an eye-height camera, the sun from the camera's side, on the flattest
//! sunlit ground near the map's mid plate camera. Each frame is drawn by the real-time renderer
//! ([`crate::look_r2::render_hero`] with the map's level, sun, sky body and look) and traced by
//! the reference path tracer at the same pose ([`regia_pt`], same scene through
//! [`crate::pt_plates::trace_capture`]).
//!
//! [`measure`] turns both into irradiance per side: every object pixel is classified by the
//! traced shading normal (sunlit side, shaded side, up, down), the terrain around the object by
//! its sun visibility (sunlit ground, the object's own shadow), and each class reports
//! `E = π · L / ρ` (lux) from the real-time HDR (full and indirect = full − direct) and from the
//! tracer (beauty and indirect = beauty − direct), with ρ the traced first-hit albedo.

use regia_core::glam::{Mat4, Quat, Vec3};
use regia_render::{JointMatrix, LabMaterial, LabMesh, LabVertex};

use crate::look_r2::R2Terrain;

/// One object of the plate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DynObject {
    /// Skinned mannequin, 0.18 grey.
    Grey,
    /// Skinned mannequin, charcoal 0.05.
    Charcoal,
    /// Rigid vehicle hull, 0.18 grey.
    Hull,
}

impl DynObject {
    /// All three, in table order.
    pub const ALL: [Self; 3] = [Self::Grey, Self::Charcoal, Self::Hull];

    /// Stable name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Grey => "mannequin_grey",
            Self::Charcoal => "mannequin_charcoal",
            Self::Hull => "hull",
        }
    }

    /// Linear albedo (neutral).
    #[must_use]
    pub fn albedo(self) -> f32 {
        match self {
            Self::Grey | Self::Hull => 0.18,
            Self::Charcoal => 0.05,
        }
    }

    /// Height the camera aims at, metres above the ground.
    #[must_use]
    pub fn aim_height(self) -> f32 {
        match self {
            Self::Grey | Self::Charcoal => 1.0,
            Self::Hull => 1.0,
        }
    }

    /// Distance from the eye to the object's centre for a plate distance `d` (the nearest
    /// surface): a mannequin's centre is 0.15 m behind its chest, the 45°-turned hull's 2.45 m
    /// behind its nearest corner.
    #[must_use]
    pub fn centre_distance(self, d: f32) -> f32 {
        match self {
            Self::Grey | Self::Charcoal => d + 0.15,
            Self::Hull => d + 2.45,
        }
    }

    /// View-depth window holding the object at plate distance `d`, metres.
    #[must_use]
    pub fn depth_window(self, d: f32) -> [f32; 2] {
        match self {
            Self::Grey | Self::Charcoal => [d - 0.45, d + 0.7],
            Self::Hull => [d - 0.3, d + 5.2],
        }
    }

    /// The large-mesh proxy the host registers for the object (`FrameRenderer::set_mesh_occluders`):
    /// the hull's oriented box on the ground at `base`; `None` for the mannequins (too small to
    /// need one).
    #[must_use]
    pub fn occluder(self, base: Vec3, yaw: f32) -> Option<regia_render::MeshOccluder> {
        (self == Self::Hull).then(|| {
            let place = Mat4::from_translation(base) * Mat4::from_rotation_y(yaw);
            regia_render::MeshOccluder::from_local_bounds(
                place.to_cols_array_2d(),
                [-HULL_HALF_LENGTH, HULL_PROFILE_Y[0], -HULL_HALF_WIDTH],
                [HULL_HALF_LENGTH, HULL_PROFILE_Y[1], HULL_HALF_WIDTH],
                [self.albedo(); 3],
            )
            .with_ground(base.y)
        })
    }

    /// The object's meshes and material, standing at `base` (ground point) and turned `yaw`
    /// radians about +Y. Material indices are 0 (one material).
    #[must_use]
    pub fn build(self, base: Vec3, yaw: f32) -> (Vec<LabMesh>, LabMaterial) {
        let a = self.albedo();
        let material = LabMaterial {
            base_color: [a, a, a, 1.0],
            metallic: 0.0,
            roughness: if self == Self::Hull { 0.55 } else { 0.8 },
            ..LabMaterial::EMPTY
        };
        let mesh = match self {
            Self::Grey | Self::Charcoal => mannequin(base, yaw),
            Self::Hull => hull(base, yaw),
        };
        (vec![mesh], material)
    }
}

// ---------------------------------------------------------------------------
// Geometry
// ---------------------------------------------------------------------------

/// A capsule from `a` to `b` (radius `r`) bound rigidly to `joint`, appended to `out`.
fn capsule(out: &mut LabMesh, a: Vec3, b: Vec3, r: f32, joint: u32) {
    const SEG: u32 = 16;
    const HALF: u32 = 6;
    let axis = b - a;
    let len = axis.length().max(1e-4);
    let rot = Quat::from_rotation_arc(Vec3::Y, axis / len);
    let base = out.vertices.len() as u32;
    // Rings from the bottom pole to the top pole; the equator ring is doubled (one at a, one at b).
    let mut rings = 0u32;
    for half in 0..2u32 {
        for i in 0..=HALF {
            let phi = if half == 0 {
                -std::f32::consts::FRAC_PI_2 + std::f32::consts::FRAC_PI_2 * i as f32 / HALF as f32
            } else {
                std::f32::consts::FRAC_PI_2 * i as f32 / HALF as f32
            };
            let y0 = if half == 0 { 0.0 } else { len };
            for s in 0..=SEG {
                let th = std::f32::consts::TAU * s as f32 / SEG as f32;
                let n = Vec3::new(phi.cos() * th.cos(), phi.sin(), phi.cos() * th.sin());
                let p = Vec3::new(0.0, y0, 0.0) + n * r;
                let wp = a + rot * p;
                let wn = (rot * n).normalize();
                let mut v = LabVertex::from_pos_normal_uv(
                    wp.to_array(),
                    wn.to_array(),
                    [s as f32 / SEG as f32, rings as f32 / (2 * HALF + 1) as f32],
                );
                v.joints = [joint, 0, 0, 0];
                v.weights = [1.0, 0.0, 0.0, 0.0];
                out.vertices.push(v);
            }
            rings += 1;
        }
    }
    let row = SEG + 1;
    for ring in 0..rings - 1 {
        for s in 0..SEG {
            let i0 = base + ring * row + s;
            let i1 = i0 + 1;
            let i2 = i0 + row;
            let i3 = i2 + 1;
            out.indices.extend_from_slice(&[i0, i2, i1, i1, i2, i3]);
        }
    }
}

/// Joints of the mannequin: (parent, pivot in the rest pose).
const JOINTS: [(Option<usize>, [f32; 3]); 11] = [
    (None, [0.0, 0.95, 0.0]),      // 0 pelvis
    (Some(0), [0.0, 1.0, 0.0]),    // 1 spine
    (Some(1), [0.0, 1.5, 0.0]),    // 2 neck / head
    (Some(1), [0.21, 1.42, 0.0]),  // 3 left upper arm
    (Some(1), [-0.21, 1.42, 0.0]), // 4 right upper arm
    (Some(3), [0.25, 1.14, 0.0]),  // 5 left forearm
    (Some(4), [-0.25, 1.14, 0.0]), // 6 right forearm
    (Some(0), [0.1, 0.92, 0.0]),   // 7 left thigh
    (Some(0), [-0.1, 0.92, 0.0]),  // 8 right thigh
    (Some(7), [0.11, 0.5, 0.0]),   // 9 left shin
    (Some(8), [-0.11, 0.5, 0.0]),  // 10 right shin
];

/// A 1.8 m skinned mannequin facing +Z (before `yaw`) with its feet at `base`: capsules bound
/// to eleven joints, in a relaxed pose (arms forward and out, elbows and one knee bent). The
/// joint palette is in world space and the model matrix is identity, as a game's skinned
/// character is submitted.
#[must_use]
pub fn mannequin(base: Vec3, yaw: f32) -> LabMesh {
    let mut mesh = LabMesh::unskinned();
    let p = |i: usize| Vec3::from_array(JOINTS[i].1);
    capsule(
        &mut mesh,
        Vec3::new(0.0, 0.95, 0.0),
        Vec3::new(0.0, 1.05, 0.0),
        0.15,
        0,
    );
    capsule(
        &mut mesh,
        Vec3::new(0.0, 1.08, 0.0),
        Vec3::new(0.0, 1.36, 0.0),
        0.17,
        1,
    );
    capsule(
        &mut mesh,
        Vec3::new(0.0, 1.45, 0.0),
        Vec3::new(0.0, 1.56, 0.0),
        0.055,
        2,
    );
    capsule(
        &mut mesh,
        Vec3::new(0.0, 1.62, 0.01),
        Vec3::new(0.0, 1.70, 0.0),
        0.10,
        2,
    );
    capsule(&mut mesh, p(3), p(5), 0.055, 3);
    capsule(&mut mesh, p(4), p(6), 0.055, 4);
    capsule(&mut mesh, p(5), Vec3::new(0.27, 0.88, 0.0), 0.045, 5);
    capsule(&mut mesh, p(6), Vec3::new(-0.27, 0.88, 0.0), 0.045, 6);
    capsule(&mut mesh, p(7), p(9), 0.08, 7);
    capsule(&mut mesh, p(8), p(10), 0.08, 8);
    capsule(&mut mesh, p(9), Vec3::new(0.11, 0.1, 0.0), 0.06, 9);
    capsule(&mut mesh, p(10), Vec3::new(-0.11, 0.1, 0.0), 0.06, 10);
    capsule(
        &mut mesh,
        Vec3::new(0.11, 0.05, -0.04),
        Vec3::new(0.11, 0.05, 0.14),
        0.045,
        9,
    );
    capsule(
        &mut mesh,
        Vec3::new(-0.11, 0.05, -0.04),
        Vec3::new(-0.11, 0.05, 0.14),
        0.045,
        10,
    );
    // The pose: local rotations about each joint's pivot (radians).
    let local: [Quat; 11] = [
        Quat::IDENTITY,
        Quat::from_rotation_y(0.08),
        Quat::from_rotation_x(-0.1),
        Quat::from_rotation_z(0.18) * Quat::from_rotation_x(-0.25),
        Quat::from_rotation_z(-0.18) * Quat::from_rotation_x(-0.15),
        Quat::from_rotation_x(-0.5),
        Quat::from_rotation_x(-0.35),
        Quat::from_rotation_x(-0.12),
        Quat::from_rotation_x(0.08),
        Quat::from_rotation_x(0.2),
        Quat::IDENTITY,
    ];
    let place = Mat4::from_translation(base) * Mat4::from_rotation_y(yaw);
    let mut world: Vec<Mat4> = Vec::with_capacity(JOINTS.len());
    for (i, (parent, pivot)) in JOINTS.iter().enumerate() {
        let pivot = Vec3::from_array(*pivot);
        let about =
            Mat4::from_translation(pivot) * Mat4::from_quat(local[i]) * Mat4::from_translation(-pivot);
        let m = match parent {
            Some(parent) => world[*parent] * about,
            None => place * about,
        };
        world.push(m);
    }
    mesh.joints = world.into_iter().map(JointMatrix::from_mat4).collect();
    mesh
}

/// The hull's half length (along its local X), metres.
const HULL_HALF_LENGTH: f32 = 2.25;
/// The hull's half width (local Z), metres.
const HULL_HALF_WIDTH: f32 = 1.2;
/// The hull's bottom and top above its base, metres.
const HULL_PROFILE_Y: [f32; 2] = [0.35, 1.8];

/// A rigid 4.5 m vehicle hull with its belly 0.35 m above `base`, its long axis along +X turned
/// `yaw`: a lower skirt slanting in toward the belly (faces looking down), vertical sides, a
/// chamfer and a flat roof (faces looking up), flat end plates. Vertices are in world space.
#[must_use]
pub fn hull(base: Vec3, yaw: f32) -> LabMesh {
    // Profile in (z, y), counter-clockwise seen from +X.
    let profile: [[f32; 2]; 8] = [
        [-0.95, 0.35],
        [0.95, 0.35],
        [1.2, 0.75],
        [1.2, 1.5],
        [0.9, 1.8],
        [-0.9, 1.8],
        [-1.2, 1.5],
        [-1.2, 0.75],
    ];
    let half_len = HULL_HALF_LENGTH;
    let place = Mat4::from_translation(base) * Mat4::from_rotation_y(yaw);
    let mut mesh = LabMesh::unskinned();
    let mut push = |p: Vec3, n: Vec3, uv: [f32; 2]| -> u32 {
        mesh.vertices.push(LabVertex::from_pos_normal_uv(
            place.transform_point3(p).to_array(),
            place.transform_vector3(n).normalize().to_array(),
            uv,
        ));
        (mesh.vertices.len() - 1) as u32
    };
    let mut indices = Vec::new();
    for k in 0..profile.len() {
        let [z0, y0] = profile[k];
        let [z1, y1] = profile[(k + 1) % profile.len()];
        // Outward normal of the edge: the profile runs counter-clockwise in (z, y).
        let n = Vec3::new(0.0, -(z1 - z0), y1 - y0).normalize();
        let a = push(Vec3::new(-half_len, y0, z0), n, [0.0, 0.0]);
        let b = push(Vec3::new(half_len, y0, z0), n, [1.0, 0.0]);
        let c = push(Vec3::new(half_len, y1, z1), n, [1.0, 1.0]);
        let d = push(Vec3::new(-half_len, y1, z1), n, [0.0, 1.0]);
        // Wind counter-clockwise seen from outside.
        let face = (Vec3::new(half_len, y0, z0) - Vec3::new(-half_len, y0, z0))
            .cross(Vec3::new(-half_len, y1, z1) - Vec3::new(-half_len, y0, z0));
        if face.dot(n) > 0.0 {
            indices.extend_from_slice(&[a, b, c, a, c, d]);
        } else {
            indices.extend_from_slice(&[a, c, b, a, d, c]);
        }
    }
    for side in [-1.0f32, 1.0] {
        let n = Vec3::new(side, 0.0, 0.0);
        let ids: Vec<u32> = profile
            .iter()
            .map(|&[z, y]| push(Vec3::new(side * half_len, y, z), n, [z, y]))
            .collect();
        // The profile is counter-clockwise in (z, y), which is clockwise seen from +X.
        for k in 1..ids.len() - 1 {
            if side > 0.0 {
                indices.extend_from_slice(&[ids[0], ids[k + 1], ids[k]]);
            } else {
                indices.extend_from_slice(&[ids[0], ids[k], ids[k + 1]]);
            }
        }
    }
    mesh.indices = indices;
    mesh
}

// ---------------------------------------------------------------------------
// Placement
// ---------------------------------------------------------------------------

/// The flattest sunlit spot within 240 m of `anchor`, for a plate `radius_m` across: the ground
/// varies least over the footprint and a march toward the sun over the heightfield does not hit
/// a ridge. Returns the ground point.
#[must_use]
pub fn find_spot(terrain: &R2Terrain, anchor: Vec3, toward_sun: Vec3, radius_m: f32) -> Vec3 {
    let mut best = (f32::MAX, anchor);
    for i in -16..=16 {
        for j in -16..=16 {
            let p = anchor + Vec3::new(i as f32 * 15.0, 0.0, j as f32 * 15.0);
            let mut lo = f32::MAX;
            let mut hi = f32::MIN;
            for k in 0..16 {
                let a = k as f32 * std::f32::consts::TAU / 16.0;
                for r in [0.0, 0.33, 0.66, 1.0] {
                    let h = terrain.height_at(p.x + a.sin() * radius_m * r, p.z + a.cos() * radius_m * r);
                    lo = lo.min(h);
                    hi = hi.max(h);
                }
            }
            let shaded = (1..120).any(|k| {
                let q = Vec3::new(p.x, hi + 2.0, p.z) + toward_sun * (k as f32 * 12.0);
                terrain.height_at(q.x, q.z) > q.y
            });
            let score = (hi - lo) + 0.002 * (p - anchor).length() + if shaded { 1.0e3 } else { 0.0 };
            if score < best.0 {
                best = (score, Vec3::new(p.x, terrain.height_at(p.x, p.z), p.z));
            }
        }
    }
    best.1
}

/// Whether the heightfield hides the direction `d` (upward) from `at`: a march of 48 steps
/// growing to 3 km.
#[must_use]
pub fn terrain_hides(terrain: &R2Terrain, at: Vec3, d: Vec3) -> bool {
    let mut t = 2.0f32;
    for _ in 0..48 {
        let p = at + d * t;
        if terrain.height_at(p.x, p.z) > p.y {
            return true;
        }
        t *= 1.15;
    }
    false
}

/// Unit horizontal vector at `azimuth` radians (x = sin, z = cos).
#[must_use]
pub fn horiz(azimuth: f32) -> Vec3 {
    Vec3::new(azimuth.sin(), 0.0, azimuth.cos())
}

/// The camera and object placement of one plate frame: the camera stands 1.7 m above the
/// ground, looks at the object with the sun 90° to its right, and the object's nearest surface is
/// `distance` metres in front of it, the pair centred on `spot`. Returns (eye, target, object
/// base, object yaw).
#[must_use]
pub fn frame_layout(
    terrain: &R2Terrain,
    spot: Vec3,
    toward_sun: Vec3,
    object: DynObject,
    distance: f32,
) -> (Vec3, Vec3, Vec3, f32) {
    let sun_az = toward_sun.x.atan2(toward_sun.z);
    // Looking along `view`, the sun's horizontal direction is the camera's right (view × up).
    let view = horiz(sun_az + std::f32::consts::FRAC_PI_2);
    let ground = |p: Vec3| terrain.height_at(p.x, p.z);
    let centre = object.centre_distance(distance);
    let eye_xz = spot - view * (centre * 0.5);
    let obj_xz = spot + view * (centre * 0.5);
    let eye = Vec3::new(eye_xz.x, ground(eye_xz) + 1.7, eye_xz.z);
    // Seated 3 cm into the ground under its centre.
    let base = Vec3::new(obj_xz.x, ground(obj_xz) - 0.03, obj_xz.z);
    let target = base + Vec3::Y * object.aim_height();
    // Mannequins face the camera; the hull is turned 45° so one long side faces the sun.
    let face_camera = (-view).x.atan2((-view).z);
    let yaw = match object {
        DynObject::Grey | DynObject::Charcoal => face_camera,
        DynObject::Hull => face_camera + std::f32::consts::FRAC_PI_4 + std::f32::consts::FRAC_PI_2,
    };
    (eye, target, base, yaw)
}

// ---------------------------------------------------------------------------
// Measurement
// ---------------------------------------------------------------------------

/// What one class of pixels is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    /// Object faces turned toward the sun (horizontal normal, sun visible).
    Sunlit,
    /// Object faces turned away from the sun (horizontal normal).
    Shaded,
    /// Object faces looking up.
    Up,
    /// Object faces looking down.
    Down,
    /// Terrain next to the object in sun.
    GroundSun,
    /// Terrain in the object's shadow.
    GroundShadow,
}

impl Side {
    /// All classes, in table order.
    pub const ALL: [Self; 6] = [
        Self::Sunlit,
        Self::Shaded,
        Self::Up,
        Self::Down,
        Self::GroundSun,
        Self::GroundShadow,
    ];

    /// Stable name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Sunlit => "sunlit",
            Self::Shaded => "shaded",
            Self::Up => "up",
            Self::Down => "down",
            Self::GroundSun => "ground_sun",
            Self::GroundShadow => "ground_shadow",
        }
    }
}

/// Irradiance sums of one class (`E = π · L / ρ`, lux, linear RGB) over its pixels.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct SideStats {
    /// Pixels in the class.
    pub pixels: usize,
    /// Real-time full frame.
    pub rt: [f64; 3],
    /// Real-time indirect (full − direct-only frame).
    pub rt_indirect: [f64; 3],
    /// Path-traced beauty.
    pub pt: [f64; 3],
    /// Path-traced indirect (beauty − direct).
    pub pt_indirect: [f64; 3],
}

impl SideStats {
    fn mean(sum: [f64; 3], n: usize) -> [f64; 3] {
        let n = n.max(1) as f64;
        sum.map(|s| s / n)
    }

    /// Mean real-time irradiance, RGB lux.
    #[must_use]
    pub fn rt_mean(&self) -> [f64; 3] {
        Self::mean(self.rt, self.pixels)
    }

    /// Mean real-time indirect irradiance.
    #[must_use]
    pub fn rt_indirect_mean(&self) -> [f64; 3] {
        Self::mean(self.rt_indirect, self.pixels)
    }

    /// Mean traced irradiance.
    #[must_use]
    pub fn pt_mean(&self) -> [f64; 3] {
        Self::mean(self.pt, self.pixels)
    }

    /// Mean traced indirect irradiance.
    #[must_use]
    pub fn pt_indirect_mean(&self) -> [f64; 3] {
        Self::mean(self.pt_indirect, self.pixels)
    }
}

/// Rec.709 luminance of a linear RGB triple.
#[must_use]
pub fn lum(c: [f64; 3]) -> f64 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

/// The inputs of [`measure`] for one frame (all row-major, top row first, same size).
pub struct MeasureInput<'a> {
    /// Real-time linear HDR, full.
    pub rt_full: &'a [[f32; 3]],
    /// Real-time linear HDR with every indirect term off.
    pub rt_direct: &'a [[f32; 3]],
    /// Traced beauty.
    pub pt_beauty: &'a [[f32; 3]],
    /// Traced direct light at the first hit.
    pub pt_direct: &'a [[f32; 3]],
    /// Traced first-hit albedo.
    pub pt_albedo: &'a [[f32; 3]],
    /// Traced first-hit shading normal.
    pub pt_normal: &'a [[f32; 3]],
    /// Traced view depth (0 for sky).
    pub pt_depth: &'a [f32],
    /// Traced sun visibility at the first hit.
    pub pt_shadow: &'a [f32],
}

/// Per-class irradiance of one frame: object pixels are the ones whose traced albedo is the
/// object's (`albedo` ± 0.006 in every channel, which also drops silhouette pixels that mixed
/// with the background) within `depth` metres; terrain pixels are the rest with an up-facing
/// normal inside the same depth window widened by 3 m.
#[must_use]
pub fn measure(input: &MeasureInput<'_>, albedo: f32, depth: [f32; 2], toward_sun: Vec3) -> [SideStats; 6] {
    let mut out = [SideStats::default(); 6];
    let sun_h = Vec3::new(toward_sun.x, 0.0, toward_sun.z).normalize_or_zero();
    let pi = std::f64::consts::PI;
    for i in 0..input.pt_beauty.len() {
        let d = input.pt_depth[i];
        if d <= 0.0 {
            continue;
        }
        let a = input.pt_albedo[i];
        let n = Vec3::from_array(input.pt_normal[i]).normalize_or_zero();
        let is_object = a.iter().all(|c| (c - albedo).abs() < 0.006) && d >= depth[0] && d <= depth[1];
        let class = if is_object {
            let nh = Vec3::new(n.x, 0.0, n.z);
            if n.y > 0.75 {
                Some(Side::Up)
            } else if n.y < -0.35 {
                Some(Side::Down)
            } else if n.y.abs() < 0.6 && nh.normalize_or_zero().dot(sun_h) > 0.5 && input.pt_shadow[i] > 0.95
            {
                Some(Side::Sunlit)
            } else if n.y.abs() < 0.6 && nh.normalize_or_zero().dot(sun_h) < -0.5 {
                Some(Side::Shaded)
            } else {
                None
            }
        } else if n.y > 0.85 && d >= depth[0] - 1.0 && d <= depth[1] + 3.0 && a.iter().all(|c| *c > 0.01) {
            let s = input.pt_shadow[i];
            if s > 0.98 {
                Some(Side::GroundSun)
            } else if s < 0.02 {
                Some(Side::GroundShadow)
            } else {
                None
            }
        } else {
            None
        };
        let Some(class) = class else { continue };
        let slot = &mut out[Side::ALL.iter().position(|c| *c == class).unwrap_or(0)];
        slot.pixels += 1;
        // Per-channel albedo for the terrain (it is tinted); the object is neutral.
        for (c, albedo_c) in a.iter().enumerate() {
            let k = pi / f64::from(albedo_c.max(1e-3));
            let rt = f64::from(input.rt_full[i][c]);
            let rtd = f64::from(input.rt_direct[i][c]);
            let pt = f64::from(input.pt_beauty[i][c]);
            let ptd = f64::from(input.pt_direct[i][c]);
            slot.rt[c] += rt * k;
            slot.rt_indirect[c] += (rt - rtd).max(0.0) * k;
            slot.pt[c] += pt * k;
            slot.pt_indirect[c] += (pt - ptd).max(0.0) * k;
        }
    }
    out
}

/// The irradiance (lux, Rec.709 luminance) the planet's sky-view table and its Lambertian ground
/// give a receiver facing `normal`, split into `[sky, ground]`: the same integral the renderer's
/// irradiance-map bake runs (`outdoor.rs` `bake_ibl`: sky table above the horizon, the preset's
/// `ground_albedo` lit by the transmitted sun and the sky below it), at 4096 cosine samples.
/// `horizon` (optional) says whether the terrain hides an upward direction; hidden sky counts
/// as ground.
#[must_use]
pub fn sky_irradiance_probe(
    planet: &regia_render::AtmosphereParameters,
    toward_sun: Vec3,
    camera_height_m: f32,
    normal: Vec3,
    horizon: Option<&dyn Fn(Vec3) -> bool>,
) -> [f64; 2] {
    let luts =
        regia_render::build_atmosphere_luts_for(planet, toward_sun.to_array(), camera_height_m.max(1.0));
    let (color, lux) = regia_render::transmitted_sun(&luts, planet);
    let sky_table = regia_render::ambient_irradiance_table(&luts);
    let cos_sun = f64::from(toward_sun.y.max(0.0));
    let ground: [f64; 3] = std::array::from_fn(|c| {
        f64::from(planet.ground_albedo[c])
            * (f64::from(color[c] * lux) * cos_sun / std::f64::consts::PI + f64::from(sky_table[c]))
    });
    let n = normal.normalize_or(Vec3::Y);
    let t = if n.y.abs() < 0.9 {
        Vec3::Y.cross(n).normalize()
    } else {
        Vec3::X.cross(n).normalize()
    };
    let b = n.cross(t);
    let count = 4096u32;
    let (mut sky, mut gnd) = ([0.0f64; 3], [0.0f64; 3]);
    for i in 0..count {
        // Hammersley point, cosine-weighted hemisphere.
        let u = (f64::from(i) + 0.5) / f64::from(count);
        let v = f64::from(i.reverse_bits()) / 4_294_967_296.0;
        let r = u.sqrt();
        let phi = std::f64::consts::TAU * v;
        let local = [r * phi.cos(), (1.0 - u).max(0.0).sqrt(), r * phi.sin()];
        let d = t * local[0] as f32 + n * local[1] as f32 + b * local[2] as f32;
        // A direction the terrain hides (`horizon`) sees the ground instead of the sky.
        let hidden = d.y >= 0.0 && horizon.is_some_and(|h| h(d));
        let sky_rad = if hidden {
            [0.0; 3]
        } else {
            regia_render::sample_sky_table(&luts, d.to_array())
        };
        for c in 0..3 {
            sky[c] += f64::from(sky_rad[c]);
            if d.y < 0.0 || hidden {
                gnd[c] += ground[c];
            }
        }
    }
    let k = std::f64::consts::PI / f64::from(count);
    [lum(sky.map(|x| x * k)), lum(gnd.map(|x| x * k))]
}

/// The sky irradiance (lux) the forward pass's terrain-horizon term removes from a receiver
/// facing `n` (`lab_forward.wgsl` `terrain_sky_occlusion`), with the sky table standing in for
/// the prefiltered environment: per diagonal sector, the band below the horizon θ_h in two
/// sub-bands seen at their middles, `L(b) · max(n·b, 0) · (sin e₁ − sin e₀) / 2` (E/π; a band's
/// solid angle is π/2 · Δ sin e). Returns `[removed sky, the bake's ground over the same
/// weights]` in lux (the forward pass adds no ground: the walls' light is the bounce grid's).
#[must_use]
pub fn shader_sky_occlusion(
    planet: &regia_render::AtmosphereParameters,
    toward_sun: Vec3,
    camera_height_m: f32,
    tan_h: [f32; 4],
    n: Vec3,
) -> [f64; 2] {
    let luts =
        regia_render::build_atmosphere_luts_for(planet, toward_sun.to_array(), camera_height_m.max(1.0));
    let (color, lux) = regia_render::transmitted_sun(&luts, planet);
    let sky_table = regia_render::ambient_irradiance_table(&luts);
    let cos_sun = toward_sun.y.max(0.0);
    let ground: [f32; 3] = std::array::from_fn(|c| {
        planet.ground_albedo[c] * (color[c] * lux * cos_sun / std::f32::consts::PI + sky_table[c])
    });
    let d = std::f32::consts::FRAC_1_SQRT_2;
    let dirs = [[d, d], [-d, d], [-d, -d], [d, -d]];
    let (mut removed, mut added) = ([0.0f64; 3], [0.0f64; 3]);
    for a in 0..4 {
        let theta = tan_h[a].max(0.0).atan();
        if theta <= 0.0 {
            continue;
        }
        for k in 0..2 {
            let (e0, e1) = (theta * 0.5 * k as f32, theta * 0.5 * (k + 1) as f32);
            let e = 0.5 * (e0 + e1);
            let b = Vec3::new(dirs[a][0] * e.cos(), e.sin(), dirs[a][1] * e.cos());
            let w = f64::from(n.dot(b).max(0.0) * (e1.sin() - e0.sin()) * 0.5);
            let l = regia_render::sample_sky_table(&luts, b.to_array());
            for c in 0..3 {
                removed[c] += f64::from(l[c]) * w;
                added[c] += f64::from(ground[c]) * w;
            }
        }
    }
    let pi = std::f64::consts::PI;
    [lum(removed) * pi, lum(added) * pi]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mannequin_is_skinned_and_closed() {
        let m = mannequin(Vec3::new(10.0, 2.0, -5.0), 0.7);
        assert_eq!(m.joints.len(), JOINTS.len());
        assert!(
            m.vertices
                .iter()
                .all(|v| (v.weights.iter().sum::<f32>() - 1.0).abs() < 1e-6)
        );
        assert!(m.vertices.iter().all(|v| (v.joints[0] as usize) < JOINTS.len()));
        assert_eq!(m.indices.len() % 3, 0);
        // The posed feet stand on the base, the head ~1.8 m above it.
        let posed: Vec<Vec3> = m
            .vertices
            .iter()
            .map(|v| Vec3::from_array(m.joints[v.joints[0] as usize].transform_point(v.position)))
            .collect();
        let lo = posed.iter().map(|p| p.y).fold(f32::MAX, f32::min);
        let hi = posed.iter().map(|p| p.y).fold(f32::MIN, f32::max);
        assert!((lo - 2.0).abs() < 0.05, "feet at {lo}");
        assert!((hi - 3.8).abs() < 0.1, "head at {hi}");
    }

    #[test]
    fn hull_normals_point_out_and_cover_up_down_and_sides() {
        let h = hull(Vec3::ZERO, 0.0);
        let centre = Vec3::new(0.0, 1.1, 0.0);
        let mut up = false;
        let mut down = false;
        for tri in h.indices.as_chunks::<3>().0 {
            let [a, b, c] =
                [tri[0], tri[1], tri[2]].map(|i| Vec3::from_array(h.vertices[i as usize].position));
            let n = Vec3::from_array(h.vertices[tri[0] as usize].normal);
            let geo = (b - a).cross(c - a).normalize();
            assert!(geo.dot(n) > 0.99, "winding agrees with the normal");
            assert!(n.dot((a + b + c) / 3.0 - centre) > 0.0, "normal points out");
            up |= n.y > 0.75;
            down |= n.y < -0.35;
        }
        assert!(up && down);
    }

    #[test]
    fn measure_classifies_by_normal_and_albedo() {
        let sun = Vec3::new(1.0, 1.0, 0.0).normalize();
        let normals = [
            [1.0, 0.0, 0.0],
            [-1.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [0.0, 1.0, 0.0],
        ];
        let albedo = [[0.18; 3], [0.18; 3], [0.18; 3], [0.6; 3]];
        let l = [[1.0f32; 3]; 4];
        let input = MeasureInput {
            rt_full: &l,
            rt_direct: &[[0.5; 3]; 4],
            pt_beauty: &l,
            pt_direct: &[[0.25; 3]; 4],
            pt_albedo: &albedo,
            pt_normal: &normals,
            pt_depth: &[3.0; 4],
            pt_shadow: &[1.0, 0.0, 1.0, 1.0],
        };
        let s = measure(&input, 0.18, [1.0, 5.0], sun);
        assert_eq!(s[0].pixels, 1);
        assert_eq!(s[1].pixels, 1);
        assert_eq!(s[2].pixels, 1);
        assert_eq!(s[4].pixels, 1);
        let e = std::f64::consts::PI / 0.18;
        assert!((s[0].rt_mean()[0] - e).abs() < 1e-3);
        assert!((s[0].rt_indirect_mean()[0] - 0.5 * e).abs() < 1e-3);
        assert!((s[0].pt_indirect_mean()[0] - 0.75 * e).abs() < 1e-3);
    }
}
