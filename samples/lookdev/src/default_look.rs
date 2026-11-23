// SPDX-License-Identifier: MIT-0
//! "Default look" shot set: DO27-like hard-surface frames rendered through the
//! engine's mesh renderer with nothing but the runtime defaults.
//!
//! The terrain shot list ([`crate::shots`]) exercises the terrain renderer. This module
//! reproduces the problems the owner saw in the game's beauty frames on the
//! [`regia_render::FrameRenderer`] path a game uses for everything that is not
//! terrain: hard-surface props on snow, a vehicle in the air, an enclosed cockpit
//! looking out, and a wide vista. Every frame uses [`RenderSettings::default`] (the
//! Desktop High profile, the default post stack, the default fog) and
//! [`Environment::default`], so a change to an engine default shows up here without
//! touching the scene. Geometry is procedural and seeded, so the frames are
//! reproducible on lavapipe and serve as goldens.
//!
//! A/B switches for look reviews (environment variables read by [`render_shot`]; unset
//! in goldens and evidence): `LOOK_DEBUG_NO_GI`, `LOOK_DEBUG_NO_SSAO`,
//! `LOOK_DEBUG_NO_SSGI`, `LOOK_DEBUG_NO_SSR`, `LOOK_DEBUG_NO_BLOOM`,
//! `LOOK_DEBUG_NO_AMBIENT` (sun only), `LOOK_DEBUG_CPU_DRAW` (no GPU culling),
//! `LOOK_DEBUG_ACES` (ACES fitted instead of AgX), `LOOK_DEBUG_CONTRAST=<f>` and
//! `LOOK_DEBUG_SAT=<f>` (look grade).

use regia_core::glam::{Mat3, Mat4, Quat, Vec3};
use regia_render::{
    DirectionalLightUniform, Environment, FrameRenderer, FrameScene, GiField, GiMode, HeightfieldGrid,
    LabMaterial, LabMesh, LabScene, LabVertex, PostSettings, ProbeSun, RenderDevice, RenderSettings,
    SdfScene, SdfShape, box_mesh, update_irradiance, uv_sphere,
};

use crate::LookdevError;

/// One camera of the default-look set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LookShot {
    /// Eye height on snow, a cluster of crates, tanks, a mast and a bunker 10–40 m away.
    Props,
    /// Looking up at a hovering hull with props and snow below.
    Vehicle,
    /// Seated in an enclosed cockpit tub, looking out through the canopy opening.
    Cockpit,
    /// Elevated wide view across rolling snow to hills 1–2 km away.
    Vista,
}

impl LookShot {
    /// Every shot, in evidence order.
    pub const ALL: [Self; 4] = [Self::Props, Self::Vehicle, Self::Cockpit, Self::Vista];

    /// Stable file stem.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Props => "props_on_snow",
            Self::Vehicle => "vehicle_in_air",
            Self::Cockpit => "cockpit",
            Self::Vista => "wide_vista",
        }
    }
}

/// How a shot is exposed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LookExposure {
    /// The default post stack's histogram auto exposure, converged over
    /// [`LookOptions::frames`] frames.
    Auto,
    /// A pinned EV100 (goldens: the histogram readback is asynchronous).
    Fixed(f32),
}

/// Frame size and exposure for [`render_shot`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LookOptions {
    /// Output width.
    pub width: u32,
    /// Output height.
    pub height: u32,
    /// Exposure mode.
    pub exposure: LookExposure,
    /// Frames rendered before the capture (auto exposure needs a few to converge).
    pub frames: u32,
}

impl LookOptions {
    /// Golden settings: small, fixed sunny-day exposure, one frame.
    #[must_use]
    pub fn golden() -> Self {
        Self {
            width: 320,
            height: 180,
            exposure: LookExposure::Fixed(SUNNY_EV100),
            frames: 1,
        }
    }

    /// [`LookOptions::golden`] captured at [`SETTLED_GOLDEN_FRAME`] instead of frame 1: the
    /// frame `default_look_golden` compares (ADR-5990). Frame 1 of a fresh renderer has no
    /// probe-GI or anti-aliasing history and is shown for one frame only.
    #[must_use]
    pub fn settled_golden() -> Self {
        Self {
            frames: SETTLED_GOLDEN_FRAME,
            ..Self::golden()
        }
    }

    /// Evidence settings: 1280×720 with converged auto exposure.
    #[must_use]
    pub fn evidence() -> Self {
        Self {
            width: 1280,
            height: 720,
            exposure: LookExposure::Auto,
            frames: 12,
        }
    }
}

/// Frame the default-look goldens capture (ADR-5990). A still view is settled from frame 40
/// on: the probe scene's screen key stops changing after 8 unchanged frames and the
/// temporally antialiased view then renders 32 held frames (two 16-sample jitter cycles)
/// before the static-screen reuse serves its cached composite. 64 leaves room if either
/// count grows; `default_look_golden` checks the capture equals the frame
/// [`SETTLE_CHECK_FRAMES`] later.
pub const SETTLED_GOLDEN_FRAME: u32 = 64;

/// How many frames after the capture `default_look_golden` draws to prove the capture is
/// settled (one 8-frame probe reuse window).
pub const SETTLE_CHECK_FRAMES: u32 = 8;

/// Fixed EV100 for the golden frames: a sunny day (sunny-16 is EV100 15).
pub const SUNNY_EV100: f32 = 14.5;

/// Sun illuminance used by every shot (clear-day noon order of magnitude).
pub const SUN_LUX: f32 = 100_000.0;

/// A built shot: the lab scene plus the GI volume the shot's probes cover.
pub struct LookScene {
    /// Geometry, camera, sun and materials.
    pub lab: LabScene,
    /// Irradiance probes around the area of interest (Desktop High runs probe GI).
    pub gi: Option<GiField>,
}

// ---------------------------------------------------------------------------
// Materials
// ---------------------------------------------------------------------------

const M_SNOW: usize = 0;
const M_PAINT_ORANGE: usize = 1;
const M_PAINT_GREY: usize = 2;
const M_HULL: usize = 3;
const M_METAL: usize = 4;
const M_CANOPY: usize = 5;
const M_INTERIOR: usize = 6;
const M_SCREEN: usize = 7;
const M_CONCRETE: usize = 8;
const M_ROCK: usize = 9;

fn mat(color: [f32; 3], metallic: f32, roughness: f32) -> LabMaterial {
    LabMaterial {
        base_color: [color[0], color[1], color[2], 1.0],
        metallic,
        roughness,
        ..LabMaterial::default()
    }
}

fn materials() -> Vec<LabMaterial> {
    let mut screen = mat([0.02, 0.02, 0.02], 0.0, 0.3);
    screen.emissive = [0.25, 0.75, 1.0];
    screen.emissive_intensity = 1500.0;
    vec![
        mat([0.80, 0.84, 0.90], 0.0, 0.62),  // snow
        mat([0.62, 0.22, 0.06], 0.0, 0.45),  // orange paint
        mat([0.32, 0.34, 0.33], 0.0, 0.55),  // grey paint
        mat([0.05, 0.05, 0.055], 0.3, 0.32), // dark hull
        mat([0.70, 0.70, 0.72], 1.0, 0.35),  // bare metal
        mat([0.02, 0.025, 0.03], 0.0, 0.05), // canopy
        mat([0.16, 0.16, 0.17], 0.0, 0.7),   // cockpit interior
        screen,
        mat([0.45, 0.44, 0.42], 0.0, 0.85), // concrete
        mat([0.20, 0.21, 0.24], 0.0, 0.9),  // dark rock
    ]
}

fn sdf_albedo(material: usize) -> [f32; 3] {
    let m = &materials()[material];
    [m.base_color[0], m.base_color[1], m.base_color[2]]
}

// ---------------------------------------------------------------------------
// Terrain stand-in
// ---------------------------------------------------------------------------

fn hash2(x: i32, z: i32) -> f32 {
    let mut h = (x as u32).wrapping_mul(0x8da6_b343) ^ (z as u32).wrapping_mul(0xd816_3841) ^ 0x2f6e_2b1d;
    h ^= h >> 13;
    h = h.wrapping_mul(0x5bd1_e995);
    h ^= h >> 15;
    (h & 0x00ff_ffff) as f32 / 16_777_215.0
}

fn value_noise(x: f32, z: f32) -> f32 {
    let (xi, zi) = (x.floor(), z.floor());
    let (fx, fz) = (x - xi, z - zi);
    let (sx, sz) = (fx * fx * (3.0 - 2.0 * fx), fz * fz * (3.0 - 2.0 * fz));
    let (xi, zi) = (xi as i32, zi as i32);
    let a = hash2(xi, zi);
    let b = hash2(xi + 1, zi);
    let c = hash2(xi, zi + 1);
    let d = hash2(xi + 1, zi + 1);
    let ab = a + (b - a) * sx;
    let cd = c + (d - c) * sx;
    ab + (cd - ab) * sz
}

fn fbm(x: f32, z: f32, octaves: u32) -> f32 {
    let (mut sum, mut amp, mut freq, mut norm) = (0.0, 0.5, 1.0, 0.0);
    for _ in 0..octaves {
        sum += amp * value_noise(x * freq, z * freq);
        norm += amp;
        amp *= 0.5;
        freq *= 2.03;
    }
    sum / norm
}

/// Ground height in metres: gentle snow drifts near the origin rising into hills
/// a few hundred metres out.
#[must_use]
pub fn ground_height(x: f32, z: f32) -> f32 {
    let drifts = (fbm(x / 35.0, z / 35.0, 4) - 0.5) * 3.0;
    let r = (x * x + z * z).sqrt();
    let t = ((r - 160.0) / 600.0).clamp(0.0, 1.0);
    let rise = t * t * (3.0 - 2.0 * t);
    let ridge = 1.0 - (fbm(x / 420.0 + 7.3, z / 420.0 - 3.1, 5) * 2.0 - 1.0).abs();
    drifts + rise * (40.0 + 190.0 * ridge * ridge)
}

fn heightfield_mesh(center: [f32; 2], size: f32, cells: u32) -> (Vec<LabVertex>, Vec<u32>) {
    let n = cells + 1;
    let step = size / cells as f32;
    let x0 = center[0] - size * 0.5;
    let z0 = center[1] - size * 0.5;
    let mut vertices = Vec::with_capacity((n * n) as usize);
    for j in 0..n {
        for i in 0..n {
            let x = x0 + i as f32 * step;
            let z = z0 + j as f32 * step;
            let e = step.max(0.5);
            let dx = ground_height(x + e, z) - ground_height(x - e, z);
            let dz = ground_height(x, z + e) - ground_height(x, z - e);
            let normal = Vec3::new(-dx, 2.0 * e, -dz).normalize();
            vertices.push(LabVertex::from_pos_normal_uv(
                [x, ground_height(x, z), z],
                normal.to_array(),
                [x / 8.0, z / 8.0],
            ));
        }
    }
    let mut indices = Vec::with_capacity((cells * cells * 6) as usize);
    for j in 0..cells {
        for i in 0..cells {
            let i0 = j * n + i;
            let i1 = i0 + 1;
            let i2 = i0 + n;
            let i3 = i2 + 1;
            indices.extend_from_slice(&[i0, i2, i1, i1, i2, i3]);
        }
    }
    (vertices, indices)
}

// ---------------------------------------------------------------------------
// Hard-surface helpers
// ---------------------------------------------------------------------------

fn cylinder_mesh(radius: f32, half_height: f32, segments: u32) -> (Vec<LabVertex>, Vec<u32>) {
    let mut v = Vec::new();
    let mut idx = Vec::new();
    for s in 0..=segments {
        let a = s as f32 / segments as f32 * std::f32::consts::TAU;
        let (c, sn) = (a.cos(), a.sin());
        for y in [-half_height, half_height] {
            v.push(LabVertex::from_pos_normal_uv(
                [radius * c, y, radius * sn],
                [c, 0.0, sn],
                [s as f32 / segments as f32, y],
            ));
        }
    }
    for s in 0..segments {
        let b0 = s * 2;
        idx.extend_from_slice(&[b0, b0 + 1, b0 + 2, b0 + 2, b0 + 1, b0 + 3]);
    }
    for (y, ny) in [(-half_height, -1.0f32), (half_height, 1.0)] {
        let centre = v.len() as u32;
        v.push(LabVertex::from_pos_normal_uv(
            [0.0, y, 0.0],
            [0.0, ny, 0.0],
            [0.5, 0.5],
        ));
        for s in 0..=segments {
            let a = s as f32 / segments as f32 * std::f32::consts::TAU;
            v.push(LabVertex::from_pos_normal_uv(
                [radius * a.cos(), y, radius * a.sin()],
                [0.0, ny, 0.0],
                [a.cos(), a.sin()],
            ));
        }
        for s in 0..segments {
            idx.extend_from_slice(&[centre, centre + 1 + s, centre + 2 + s]);
        }
    }
    (v, idx)
}

/// Flips triangles whose winding disagrees with their vertex normals, so every
/// generated mesh is counter-clockwise seen from outside.
fn fix_winding(vertices: &[LabVertex], indices: &mut [u32]) {
    for tri in indices.as_chunks_mut::<3>().0.iter_mut() {
        let p = |i: u32| Vec3::from_array(vertices[i as usize].position);
        let n = Vec3::from_array(vertices[tri[0] as usize].normal)
            + Vec3::from_array(vertices[tri[1] as usize].normal)
            + Vec3::from_array(vertices[tri[2] as usize].normal);
        let geo = (p(tri[1]) - p(tri[0])).cross(p(tri[2]) - p(tri[0]));
        if geo.dot(n) < 0.0 {
            tri.swap(1, 2);
        }
    }
}

/// Bakes `transform` into the vertices (normals by the inverse transpose), so the
/// model matrix stays identity and non-uniform scale shades correctly.
fn placed(mesh: (Vec<LabVertex>, Vec<u32>), transform: Mat4, material: usize) -> LabMesh {
    let (mut vertices, mut indices) = mesh;
    let normal_m = Mat3::from_mat4(transform).inverse().transpose();
    let tangent_m = Mat3::from_mat4(transform);
    for v in &mut vertices {
        v.position = transform
            .transform_point3(Vec3::from_array(v.position))
            .to_array();
        v.normal = (normal_m * Vec3::from_array(v.normal))
            .normalize_or_zero()
            .to_array();
        let t = (tangent_m * Vec3::new(v.tangent[0], v.tangent[1], v.tangent[2])).normalize_or_zero();
        v.tangent = [t.x, t.y, t.z, v.tangent[3]];
    }
    fix_winding(&vertices, &mut indices);
    LabMesh {
        vertices,
        indices,
        material,
        model: Mat4::IDENTITY.to_cols_array_2d(),
        joints: Vec::new(),
    }
}

fn trs(translation: [f32; 3], yaw_deg: f32, pitch_deg: f32, roll_deg: f32, scale: [f32; 3]) -> Mat4 {
    let rotation = Quat::from_euler(
        regia_core::glam::EulerRot::YXZ,
        yaw_deg.to_radians(),
        pitch_deg.to_radians(),
        roll_deg.to_radians(),
    );
    Mat4::from_scale_rotation_translation(Vec3::from_array(scale), rotation, Vec3::from_array(translation))
}

/// Builder that keeps the render meshes and the GI SDF in step.
struct World {
    meshes: Vec<LabMesh>,
    sdf: SdfScene,
}

impl World {
    fn new() -> Self {
        Self {
            meshes: Vec::new(),
            sdf: SdfScene::default(),
        }
    }

    /// An axis-aligned box resting with its base at `base` (y = bottom face).
    fn block(&mut self, base: [f32; 3], half: [f32; 3], yaw: f32, material: usize) {
        let centre = [base[0], base[1] + half[1], base[2]];
        self.meshes.push(placed(
            box_mesh(half),
            trs(centre, yaw, 0.0, 0.0, [1.0; 3]),
            material,
        ));
        // The GI SDF is axis-aligned; a yawed box is approximated by its unrotated
        // extents, which is close enough for diffuse probe occlusion.
        self.sdf.shapes.push(SdfShape::Box {
            center: centre,
            half,
            albedo: sdf_albedo(material),
            emissive: [0.0; 3],
        });
    }

    fn mesh(&mut self, mesh: LabMesh) {
        self.meshes.push(mesh);
    }

    /// A prop resting on the ground at `(x, z)`.
    fn on_ground(&mut self, x: f32, z: f32, half: [f32; 3], yaw: f32, material: usize) {
        let y = ground_height(x, z) - 0.15;
        self.block([x, y, z], half, yaw, material);
    }

    fn tank(&mut self, x: f32, z: f32, radius: f32, length: f32, yaw: f32) {
        let y = ground_height(x, z) + radius + 0.35;
        self.mesh(placed(
            cylinder_mesh(radius, length * 0.5, 28),
            trs([x, y, z], yaw, 0.0, 90.0, [1.0; 3]),
            M_PAINT_GREY,
        ));
        for end in [-0.32f32, 0.32] {
            let (s, c) = yaw.to_radians().sin_cos();
            let off = end * length;
            self.on_ground(x + c * off, z - s * off, [0.25, 0.4, radius * 0.9], yaw, M_METAL);
        }
        self.sdf.shapes.push(SdfShape::Box {
            center: [x, y, z],
            half: [length * 0.5, radius, radius],
            albedo: sdf_albedo(M_PAINT_GREY),
            emissive: [0.0; 3],
        });
    }

    fn mast(&mut self, x: f32, z: f32, height: f32) {
        let y0 = ground_height(x, z) - 0.2;
        let leg = 1.1;
        for (dx, dz) in [(-leg, -leg), (leg, -leg), (-leg, leg), (leg, leg)] {
            self.block([x + dx, y0, z + dz], [0.07, height * 0.5, 0.07], 0.0, M_METAL);
        }
        let rungs = (height / 2.5) as u32;
        for r in 1..=rungs {
            let y = y0 + r as f32 * 2.5;
            self.block([x, y, z - leg], [leg, 0.05, 0.05], 0.0, M_METAL);
            self.block([x, y, z + leg], [leg, 0.05, 0.05], 0.0, M_METAL);
            self.block([x - leg, y, z], [0.05, 0.05, leg], 0.0, M_METAL);
            self.block([x + leg, y, z], [0.05, 0.05, leg], 0.0, M_METAL);
        }
        self.block([x, y0 + height, z], [1.5, 0.6, 1.5], 0.0, M_PAINT_ORANGE);
    }

    fn crate_stack(&mut self, x: f32, z: f32, yaw: f32) {
        self.on_ground(x, z, [1.2, 1.2, 1.2], yaw, M_PAINT_ORANGE);
        self.on_ground(x + 2.6, z + 0.3, [1.2, 1.2, 1.2], yaw + 8.0, M_PAINT_GREY);
        let top = ground_height(x, z) + 2.25;
        self.block(
            [x + 1.2, top, z + 0.1],
            [1.1, 1.1, 1.1],
            yaw - 12.0,
            M_PAINT_ORANGE,
        );
    }

    fn bunker(&mut self, x: f32, z: f32) {
        self.on_ground(x, z, [5.0, 1.6, 3.5], 0.0, M_CONCRETE);
        let roof = ground_height(x, z) + 3.0;
        self.block([x, roof, z], [5.4, 0.25, 3.9], 0.0, M_CONCRETE);
        self.block([x - 5.05, roof - 2.2, z], [0.05, 0.6, 1.4], 0.0, M_METAL);
        self.on_ground(x + 6.5, z - 1.0, [0.6, 0.5, 2.5], 0.0, M_CONCRETE);
    }

    fn rocks(&mut self, seed: i32, count: i32, centre: [f32; 2], spread: f32) {
        for i in 0..count {
            let x = centre[0] + (hash2(seed, i) - 0.5) * spread;
            let z = centre[1] + (hash2(i, seed) - 0.5) * spread;
            let s = 0.4 + 1.6 * hash2(seed + 3, i * 7);
            let y = ground_height(x, z) + s * 0.25;
            self.mesh(placed(
                uv_sphere(1.0, 10, 6),
                trs([x, y, z], hash2(i, 9) * 360.0, 10.0, 0.0, [s * 1.4, s * 0.7, s]),
                M_ROCK,
            ));
        }
    }

    fn props(&mut self) {
        self.crate_stack(14.0, -4.0, 18.0);
        self.tank(22.0, 6.0, 1.5, 7.0, -25.0);
        self.mast(30.0, -10.0, 14.0);
        self.bunker(42.0, 4.0);
        self.on_ground(9.0, 5.0, [0.4, 0.6, 2.0], 70.0, M_CONCRETE);
        self.on_ground(11.0, 9.0, [0.4, 0.6, 2.0], 50.0, M_CONCRETE);
        self.rocks(11, 14, [26.0, 0.0], 50.0);
    }

    fn vehicle(&mut self, centre: [f32; 3], yaw: f32) {
        let hull = trs(centre, yaw, 0.0, 0.0, [1.0; 3]);
        let part = |t: [f32; 3], yaw_l: f32, pitch: f32, roll: f32, s: [f32; 3]| {
            hull * trs(t, yaw_l, pitch, roll, s)
        };
        self.mesh(placed(
            uv_sphere(1.0, 40, 20),
            part([0.0, 0.0, 0.0], 0.0, 0.0, 0.0, [7.5, 1.1, 3.2]),
            M_HULL,
        ));
        for side in [-1.0f32, 1.0] {
            self.mesh(placed(
                box_mesh([3.2, 0.12, 2.6]),
                part([-1.0, -0.1, side * 4.6], side * 12.0, 0.0, side * -8.0, [1.0; 3]),
                M_HULL,
            ));
            self.mesh(placed(
                cylinder_mesh(0.7, 1.6, 20),
                part([-3.6, 0.35, side * 2.4], 0.0, 0.0, 90.0, [1.0; 3]),
                M_METAL,
            ));
        }
        self.mesh(placed(
            uv_sphere(1.0, 24, 12),
            part([1.8, 0.9, 0.0], 0.0, 0.0, 0.0, [1.8, 0.6, 1.0]),
            M_CANOPY,
        ));
        self.mesh(placed(
            box_mesh([0.6, 0.6, 0.08]),
            part([-5.6, 1.1, 0.0], 0.0, 0.0, -20.0, [1.0; 3]),
            M_HULL,
        ));
    }

    /// An enclosed cockpit tub whose seated eye is at `origin + (−0.4, 1.15, 0)`,
    /// open to the front (+x) through a canopy frame.
    fn cockpit(&mut self, origin: [f32; 3]) {
        let o = Vec3::from_array(origin);
        let at = |x: f32, y: f32, z: f32| (o + Vec3::new(x, y, z)).to_array();
        self.block(at(0.2, -0.1, 0.0), [1.8, 0.1, 1.0], 0.0, M_INTERIOR); // floor
        for side in [-1.0f32, 1.0] {
            self.block(at(0.2, 0.0, side * 1.0), [1.8, 0.75, 0.06], 0.0, M_INTERIOR); // side walls
            // Canopy rails from the roof edge down to the dashboard.
            self.mesh(placed(
                box_mesh([0.82, 0.05, 0.05]),
                trs(at(1.0, 1.15, side * 0.94), 0.0, 0.0, -33.0, [1.0; 3]),
                M_HULL,
            ));
        }
        self.block(at(-1.55, 0.0, 0.0), [0.06, 0.8, 1.0], 0.0, M_INTERIOR); // back wall
        self.block(at(-0.65, 1.5, 0.0), [0.95, 0.06, 1.05], 0.0, M_INTERIOR); // roof
        self.block(at(1.65, 0.0, 0.0), [0.35, 0.42, 0.95], 0.0, M_INTERIOR); // dashboard
        self.mesh(placed(
            box_mesh([0.82, 0.04, 0.04]),
            trs(at(1.0, 1.15, 0.0), 0.0, 0.0, -33.0, [1.0; 3]),
            M_HULL,
        )); // centre canopy bar
        for (i, z) in [-0.55f32, 0.0, 0.55].into_iter().enumerate() {
            let m = if i == 1 { M_SCREEN } else { M_INTERIOR };
            self.mesh(placed(
                box_mesh([0.02, 0.14, 0.2]),
                trs(at(1.29, 0.62, z), 0.0, 0.0, 0.0, [1.0; 3]),
                m,
            ));
        }
        self.block(at(-0.75, 0.0, 0.0), [0.35, 0.45, 0.45], 0.0, M_PAINT_GREY); // seat
        self.block(at(-1.05, 0.45, 0.0), [0.08, 0.5, 0.42], 0.0, M_PAINT_GREY); // seat back
        // Exterior shell so the tub reads as a vehicle in other shots.
        self.block(at(0.2, -0.45, 0.0), [2.2, 0.35, 1.3], 0.0, M_HULL);
    }
}

fn sun_direction(elevation_deg: f32, azimuth_deg: f32) -> [f32; 3] {
    let (el, az) = (elevation_deg.to_radians(), azimuth_deg.to_radians());
    let toward_sun = Vec3::new(el.cos() * az.cos(), el.sin(), el.cos() * az.sin());
    (-toward_sun).to_array()
}

fn ground_meshes(world: &mut World, far: bool) {
    world.mesh(placed(
        heightfield_mesh([20.0, 0.0], 220.0, 160),
        Mat4::IDENTITY,
        M_SNOW,
    ));
    if far {
        // A coarse ring out to 2.5 km for the vista; the near patch sits on top of it
        // (pushed down a little so the two never z-fight inside the near patch).
        world.mesh(placed(
            heightfield_mesh([600.0, 0.0], 5000.0, 220),
            Mat4::from_translation(Vec3::new(0.0, -0.6, 0.0)),
            M_SNOW,
        ));
    }
}

fn ground_sdf(centre: [f32; 2], size: f32) -> SdfShape {
    let cells = 64u32;
    let step = size / cells as f32;
    let x0 = centre[0] - size * 0.5;
    let z0 = centre[1] - size * 0.5;
    let mut samples = Vec::with_capacity(((cells + 1) * (cells + 1)) as usize);
    for j in 0..=cells {
        for i in 0..=cells {
            samples.push(ground_height(x0 + i as f32 * step, z0 + j as f32 * step));
        }
    }
    SdfShape::Heightfield(HeightfieldGrid {
        origin_x: x0,
        origin_z: z0,
        spacing_x: step,
        spacing_z: step,
        width: cells + 1,
        height: cells + 1,
        samples,
        albedo: sdf_albedo(M_SNOW),
    })
}

/// Eye, target, far plane, sun direction and GI box (origin, spacing, dims) of a shot.
type ShotSetup = ([f32; 3], [f32; 3], f32, [f32; 3], ([f32; 3], [f32; 3], [u32; 3]));

/// Builds `shot` at `width`×`height` with the engine's runtime defaults.
///
/// # Errors
///
/// [`LookdevError::Config`] when the GI grid is rejected.
pub fn build_shot(shot: LookShot, width: u32, height: u32) -> Result<LookScene, LookdevError> {
    let mut world = World::new();
    let (eye, target, far, sun, gi_box): ShotSetup = match shot {
        LookShot::Props => {
            ground_meshes(&mut world, true);
            world.props();
            let eye = [-6.0, ground_height(-6.0, 2.0) + 1.75, 2.0];
            (
                eye,
                [24.0, ground_height(24.0, 0.0) + 2.5, -1.0],
                3000.0,
                sun_direction(24.0, -55.0),
                (
                    [4.0, ground_height(14.0, 0.0) - 0.5, -20.0],
                    [6.0, 3.0, 6.0],
                    [8, 4, 8],
                ),
            )
        }
        LookShot::Vehicle => {
            ground_meshes(&mut world, true);
            world.props();
            world.vehicle([12.0, ground_height(12.0, 0.0) + 11.0, 1.0], -30.0);
            let eye = [-8.0, ground_height(-8.0, 3.0) + 1.7, 3.0];
            (
                eye,
                [12.0, ground_height(12.0, 0.0) + 8.5, 0.0],
                3000.0,
                sun_direction(38.0, -35.0),
                (
                    [0.0, ground_height(12.0, 0.0) - 0.5, -20.0],
                    [5.0, 4.0, 6.0],
                    [8, 4, 8],
                ),
            )
        }
        LookShot::Cockpit => {
            ground_meshes(&mut world, true);
            world.props();
            world.vehicle([30.0, ground_height(30.0, 10.0) + 14.0, 10.0], 160.0);
            let origin = [0.0, ground_height(0.0, 0.0) + 0.8, 0.0];
            world.cockpit(origin);
            let eye = [origin[0] - 0.4, origin[1] + 1.15, origin[2]];
            (
                eye,
                [eye[0] + 10.0, eye[1] - 0.9, eye[2]],
                3000.0,
                sun_direction(30.0, 35.0),
                (
                    [origin[0] - 1.6, origin[1] - 0.05, origin[2] - 0.9],
                    [0.5, 0.4, 0.3],
                    [8, 4, 7],
                ),
            )
        }
        LookShot::Vista => {
            ground_meshes(&mut world, true);
            world.props();
            world.rocks(23, 30, [120.0, 40.0], 200.0);
            world.vehicle([70.0, ground_height(70.0, -20.0) + 25.0, -20.0], -60.0);
            let eye = [-40.0, ground_height(-40.0, 0.0) + 32.0, 0.0];
            (
                eye,
                [400.0, ground_height(400.0, 60.0) + 10.0, 60.0],
                4000.0,
                sun_direction(16.0, -75.0),
                (
                    [4.0, ground_height(14.0, 0.0) - 0.5, -20.0],
                    [6.0, 3.0, 6.0],
                    [8, 4, 8],
                ),
            )
        }
    };
    world.sdf.shapes.push(ground_sdf([20.0, 0.0], 200.0));

    let directional = DirectionalLightUniform::new(sun, [1.0, 0.97, 0.92], SUN_LUX);
    let (origin, spacing, dims) = gi_box;
    let mut field =
        GiField::new(origin, spacing, dims).map_err(|e| LookdevError::Config(format!("gi grid: {e}")))?;
    // Three updates: each one adds a bounce (hits read the previous field), the way a
    // game converges the volume over its first frames.
    for _ in 0..3 {
        update_irradiance(
            &mut field,
            &world.sdf,
            ProbeSun {
                direction: sun,
                color: [1.0, 0.97, 0.92],
                illuminance: SUN_LUX,
            },
            64,
            0.0,
        );
    }

    let env = Environment::default();
    let lab = LabScene {
        width,
        height,
        eye,
        target,
        fov_y: if shot == LookShot::Cockpit {
            70.0_f32.to_radians()
        } else {
            55.0_f32.to_radians()
        },
        near: 0.1,
        far,
        depth_prepass: true,
        directional_shadows: true,
        ibl: env.ibl,
        exposure_ev: SUNNY_EV100,
        ambient_lux: env.ambient_lux,
        double_sided: env.double_sided,
        post: None,
        directional,
        local_lights: Vec::new(),
        materials: materials(),
        meshes: world.meshes,
    };
    Ok(LookScene { lab, gi: Some(field) })
}

/// Renders `shot` with the runtime defaults and returns RGBA8 pixels (sRGB-encoded,
/// top row first) plus the EV100 the frame was exposed at (`None` under auto exposure).
///
/// # Errors
///
/// Scene building or the frame submission failed.
pub fn render_shot(
    device: &RenderDevice,
    renderer: &mut FrameRenderer,
    shot: LookShot,
    options: LookOptions,
) -> Result<Vec<u8>, LookdevError> {
    render_shot_with(
        device,
        renderer,
        shot,
        options,
        &RenderSettings::default(),
        &|_| {},
    )
}

/// [`render_shot`] at chosen settings, after `tweak` edits the scene (`mobile_forward_bench` runs
/// the Mobile rows and adds local lights).
///
/// # Errors
///
/// [`LookdevError`] when the shot or a frame fails.
pub fn render_shot_with(
    device: &RenderDevice,
    renderer: &mut FrameRenderer,
    shot: LookShot,
    options: LookOptions,
    settings: &RenderSettings,
    tweak: &dyn Fn(&mut regia_render::LabScene),
) -> Result<Vec<u8>, LookdevError> {
    let mut frames = render_frames(
        device,
        renderer,
        shot,
        options,
        settings,
        tweak,
        &[options.frames.max(1)],
    )?;
    frames
        .pop()
        .ok_or_else(|| LookdevError::Config("no frame captured".into()))
}

/// Renders `shot` like [`render_shot`] for `max(captures)` frames on one renderer and returns
/// the pixels of each listed frame (1-based, in the order given). `options.frames` is
/// ignored. `default_look_golden` uses it to capture frame [`SETTLED_GOLDEN_FRAME`] and the
/// frame [`SETTLE_CHECK_FRAMES`] later in one run.
///
/// # Errors
///
/// [`LookdevError::Config`] when `captures` is empty or lists frame 0; otherwise as
/// [`render_shot`].
pub fn render_shot_captures(
    device: &RenderDevice,
    renderer: &mut FrameRenderer,
    shot: LookShot,
    options: LookOptions,
    captures: &[u32],
) -> Result<Vec<Vec<u8>>, LookdevError> {
    render_frames(
        device,
        renderer,
        shot,
        options,
        &RenderSettings::default(),
        &|_| {},
        captures,
    )
}

/// Checks a capture list: not empty, no frame 0 (frames are 1-based).
fn check_captures(captures: &[u32]) -> Result<u32, LookdevError> {
    match captures.iter().copied().max() {
        None => Err(LookdevError::Config("no capture frame listed".into())),
        Some(_) if captures.contains(&0) => Err(LookdevError::Config(
            "capture frames are 1-based; frame 0 is not drawn".into(),
        )),
        Some(last) => Ok(last),
    }
}

/// The body of [`render_shot_with`] and [`render_shot_captures`].
fn render_frames(
    device: &RenderDevice,
    renderer: &mut FrameRenderer,
    shot: LookShot,
    options: LookOptions,
    settings: &RenderSettings,
    tweak: &dyn Fn(&mut regia_render::LabScene),
    captures: &[u32],
) -> Result<Vec<Vec<u8>>, LookdevError> {
    let last = check_captures(captures)?;
    let scene = build_shot(shot, options.width, options.height)?;
    let mut post: PostSettings = settings.resolve_post();
    // Desktop High resolves TAA in the frame renderer; the post stack's single-frame
    // TAA would only blur on top of it.
    post.taa = false;
    if let Ok(v) = std::env::var("LOOK_DEBUG_SAT") {
        post.saturation = v.parse().unwrap_or(post.saturation);
    }
    if let Ok(v) = std::env::var("LOOK_DEBUG_CONTRAST") {
        post.contrast = v.parse().unwrap_or(post.contrast);
    }
    if std::env::var("LOOK_DEBUG_ACES").is_ok() {
        post.tonemap = regia_render::TonemapOperator::AcesFitted;
    }
    if std::env::var("LOOK_DEBUG_NO_BLOOM").is_ok() {
        post.bloom = false;
    }
    match options.exposure {
        LookExposure::Auto => post.auto_exposure = true,
        LookExposure::Fixed(ev) => {
            post.auto_exposure = false;
            post.exposure_ev = ev;
        }
    }
    let mut quality = settings.quality();
    if std::env::var("LOOK_DEBUG_NO_SSGI").is_ok() {
        quality.screen_space_gi = false;
    }
    if std::env::var("LOOK_DEBUG_CPU_DRAW").is_ok() {
        quality.gpu_driven = false;
    }
    if std::env::var("LOOK_DEBUG_NO_SSAO").is_ok() {
        quality.ssao = regia_render::SsaoMode::Off;
    }
    if std::env::var("LOOK_DEBUG_NO_SSR").is_ok() {
        quality.ssr = false;
    }
    if quality.gi == GiMode::Probes {
        renderer.set_gi_field(scene.gi.clone());
    } else {
        renderer.set_gi_field(None);
    }
    let mut lab = scene.lab;
    tweak(&mut lab);
    if std::env::var("LOOK_DEBUG_NO_GI").is_ok() {
        renderer.set_gi_field(None);
    }
    if std::env::var("LOOK_DEBUG_NO_AMBIENT").is_ok() {
        lab.ibl = false;
        lab.ambient_lux = 0.0;
        renderer.set_gi_field(None);
    }
    lab.exposure_ev = post.exposure_ev;
    lab.depth_prepass = settings.profile.depth_prepass;
    lab.post = Some(post);
    let mut frame: FrameScene<'_> = lab.as_frame_scene_with_quality(quality);
    frame.fog = settings.fog;
    let mut captured: Vec<Option<Vec<u8>>> = vec![None; captures.len()];
    for index in 1..=last {
        let _span = tracing::trace_span!("default_look_frame", shot = shot.name(), index).entered();
        let pixels = renderer.render_offscreen(device, &frame)?.pixels;
        for (slot, _) in captured.iter_mut().zip(captures).filter(|(_, at)| **at == index) {
            *slot = Some(pixels.clone());
        }
    }
    captured
        .into_iter()
        .map(|pixels| pixels.ok_or_else(|| LookdevError::Config("capture frame not drawn".into())))
        .collect()
}

/// Writes tightly packed RGBA8 as a PNG.
///
/// # Errors
///
/// [`LookdevError::Io`] / [`LookdevError::Png`].
pub fn write_png(path: &std::path::Path, width: u32, height: u32, rgba: &[u8]) -> Result<(), LookdevError> {
    let file = std::fs::File::create(path).map_err(|source| LookdevError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .map_err(|e| LookdevError::Png(e.to_string()))?;
    writer
        .write_image_data(rgba)
        .map_err(|e| LookdevError::Png(e.to_string()))?;
    writer.finish().map_err(|e| LookdevError::Png(e.to_string()))
}

/// Reads an 8-bit RGBA or RGB PNG into tightly packed RGBA8.
///
/// # Errors
///
/// [`LookdevError::Io`] / [`LookdevError::Png`].
pub fn read_png(path: &std::path::Path) -> Result<(u32, u32, Vec<u8>), LookdevError> {
    let file = std::fs::File::open(path).map_err(|source| LookdevError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let decoder = png::Decoder::new(std::io::BufReader::new(file));
    let mut reader = decoder
        .read_info()
        .map_err(|e| LookdevError::Png(e.to_string()))?;
    let size = reader
        .output_buffer_size()
        .ok_or_else(|| LookdevError::Png("png too large".into()))?;
    let mut buf = vec![0; size];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|e| LookdevError::Png(e.to_string()))?;
    let (w, h) = (info.width, info.height);
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf[..(w * h * 4) as usize].to_vec(),
        png::ColorType::Rgb => buf[..(w * h * 3) as usize]
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|c| [c[0], c[1], c[2], 255])
            .collect(),
        other => return Err(LookdevError::Png(format!("unsupported color type {other:?}"))),
    };
    Ok((w, h, rgba))
}

/// Places `left` and `right` (same size) side by side with a 4 px divider.
#[must_use]
pub fn side_by_side(width: u32, height: u32, left: &[u8], right: &[u8]) -> (u32, Vec<u8>) {
    let gap = 4u32;
    let out_w = width * 2 + gap;
    let mut out = vec![255u8; (out_w * height * 4) as usize];
    for y in 0..height {
        let row = (y * width * 4) as usize..((y + 1) * width * 4) as usize;
        let dst_l = (y * out_w * 4) as usize;
        out[dst_l..dst_l + (width * 4) as usize].copy_from_slice(&left[row.clone()]);
        let dst_r = dst_l + ((width + gap) * 4) as usize;
        out[dst_r..dst_r + (width * 4) as usize].copy_from_slice(&right[row]);
    }
    (out_w, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_meshes_face_outward() {
        let (v, mut i) = cylinder_mesh(1.0, 1.0, 12);
        fix_winding(&v, &mut i);
        for tri in i.as_chunks::<3>().0 {
            let p = |k: u32| Vec3::from_array(v[k as usize].position);
            let geo = (p(tri[1]) - p(tri[0])).cross(p(tri[2]) - p(tri[0]));
            assert!(geo.dot(Vec3::from_array(v[tri[0] as usize].normal)) >= 0.0);
        }
        let (hv, hi) = heightfield_mesh([0.0, 0.0], 10.0, 4);
        let p = |k: u32| Vec3::from_array(hv[k as usize].position);
        let geo = (p(hi[1]) - p(hi[0])).cross(p(hi[2]) - p(hi[0]));
        assert!(geo.y > 0.0, "ground triangles face up");
    }

    #[test]
    fn settled_golden_captures_past_the_settle_point() {
        // 8 probe-reuse frames + 32 held converge frames: frame 40 is the first settled one
        // (ADR-5990). The golden must capture at or after it, with the same size and exposure.
        let settled = LookOptions::settled_golden();
        assert!(
            settled.frames >= 40,
            "capture frame {} is not settled",
            settled.frames
        );
        assert_eq!(settled.frames, SETTLED_GOLDEN_FRAME);
        assert_eq!(LookOptions { frames: 1, ..settled }, LookOptions::golden());
        const { assert!(SETTLE_CHECK_FRAMES >= 1) };
    }

    #[test]
    fn capture_lists_are_checked() {
        assert!(matches!(check_captures(&[]), Err(LookdevError::Config(_))));
        assert!(matches!(check_captures(&[0, 4]), Err(LookdevError::Config(_))));
        assert_eq!(check_captures(&[72, 64]).ok(), Some(72));
        assert_eq!(check_captures(&[1]).ok(), Some(1));
    }

    #[test]
    fn every_shot_builds_with_a_gi_volume() {
        for shot in LookShot::ALL {
            let scene = build_shot(shot, 64, 36).expect("shot builds");
            assert!(!scene.lab.meshes.is_empty());
            let gi = scene.gi.expect("gi volume");
            assert!(gi.probe_count() <= regia_render::MAX_PROBES);
        }
    }
}
